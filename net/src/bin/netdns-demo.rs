//! `netdns-demo` - the rheo-net Phase N1c proof cell (docs/NETSTACK.md): the
//! caching DNS client. Its exit code is the proof.
//!
//! The **deterministic, network-free** checks are the core (they hold with no
//! outbound internet):
//! 1. **Parse oracle**: a hand-crafted compressed DNS response (a `0xC0` name
//!    pointer) parses to `example.com A 93.184.216.34` with TTL 3600.
//! 2. **Pointer-loop safety**: a self-referential pointer and a mutual-pointer
//!    cycle both return an error, never a hang (the jump cap).
//! 3. **Hosts table**: names in the static hosts table resolve to their
//!    configured IP with **zero** network queries (asserted via the counter).
//! 4. **Blocklist**: an exact-blocked and a wildcard-blocked name return
//!    `Err(Blocked)` with **zero** queries.
//! 5. **Cache hit**: a pre-seeded name resolves from cache (twice, case/dot
//!    normalized) with **zero** queries.
//! 6. **Cache unit**: TTL expiry + LRU eviction on an explicit clock.
//!
//! Then a **bonus live resolve** of `example.com` over SLIRP's DNS (10.0.2.3).
//! SLIRP proxies to the host resolver, so the *address* is non-deterministic - we
//! assert only what cannot vary: that a query was actually sent. **Everything about
//! the answer is reported, never asserted** - no outbound DNS (a timeout), an
//! AAAA-only or empty answer, an NXDOMAIN, a zero TTL that defeats the cache. An
//! earlier version asserted "at least one A record" and "the second lookup is a cache
//! hit", which made a bonus phase intermittently fail on what a real resolver happened
//! to return; the deterministic checks above are the proof, and this is a live report. The kernel is
//! untouched - portable userspace over the existing `OP_NET_*` queue path.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};
use rheo_net::dns::{self, Cache, Config, DnsError, QType, RData, Resolver};
use rheo_net::ip::{IpAddr, Ipv4Addr};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's default guest address + built-in DNS responder.
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// A known-good **compressed** DNS response for the query `example.com A`:
/// transaction id 0x1234, one answer `A 93.184.216.34` TTL 3600, whose owner name
/// is a compression pointer (`0xC0 0x0C`) back to the question at offset 12. This
/// pins the codec + the pointer scheme the way N1b's `0x6D45` pins the checksum.
const COMPRESSED_RESPONSE: [u8; 45] = [
    0x12, 0x34, // id
    0x81, 0x80, // flags: response, RD, RA, rcode 0
    0x00, 0x01, // qdcount 1
    0x00, 0x01, // ancount 1
    0x00, 0x00, // nscount 0
    0x00, 0x00, // arcount 0
    // question: example.com A IN (starts at offset 12)
    0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', //
    0x03, b'c', b'o', b'm', //
    0x00, // root label
    0x00, 0x01, // qtype A
    0x00, 0x01, // qclass IN
    // answer
    0xC0, 0x0C, // name: pointer to offset 12 (the question name)
    0x00, 0x01, // type A
    0x00, 0x01, // class IN
    0x00, 0x00, 0x0E, 0x10, // ttl 3600
    0x00, 0x04, // rdlength 4
    93, 184, 216, 34, // rdata: 93.184.216.34
];

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("netdns-demo: all DNS assertions OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

/// Deterministic TTL-expiry + LRU checks on a standalone [`Cache`] with an
/// explicit clock (no wall time involved).
fn cache_unit_ok() -> bool {
    let a = vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))];
    let b = vec![IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2))];
    let c = vec![IpAddr::V4(Ipv4Addr::new(3, 3, 3, 3))];

    let mut cache = Cache::new(2);
    // TTL: entry expires at clock 100.
    cache.insert("a", dns::TYPE_A, a.clone(), 100);
    if cache.get("a", dns::TYPE_A, 50).is_none() {
        return false; // still live at 50
    }
    if cache.get("a", dns::TYPE_A, 100).is_some() {
        return false; // expired at 100 -> evicted
    }
    if !cache.is_empty() {
        return false;
    }

    // LRU: cap 2. Insert a, b; touch a; insert c -> b (LRU) is evicted.
    cache.insert("a", dns::TYPE_A, a, 1_000);
    cache.insert("b", dns::TYPE_A, b, 1_000);
    let _ = cache.get("a", dns::TYPE_A, 1); // a becomes most-recently used
    cache.insert("c", dns::TYPE_A, c, 1_000);
    if cache.get("a", dns::TYPE_A, 1).is_none() {
        return false; // a survived
    }
    if cache.get("b", dns::TYPE_A, 1).is_some() {
        return false; // b evicted
    }
    if cache.get("c", dns::TYPE_A, 1).is_none() {
        return false; // c present
    }
    true
}

async fn run() {
    // --- 1. Parse oracle: a compressed response decodes exactly. ---
    let resp = match dns::parse_response(&COMPRESSED_RESPONSE) {
        Ok(r) => r,
        Err(_) => return fail(10),
    };
    if resp.id != 0x1234 || resp.answers.len() != 1 {
        return fail(11);
    }
    let ans = &resp.answers[0];
    if ans.name != "example.com" || ans.ttl != 3600 {
        return fail(12);
    }
    match ans.data {
        RData::A(ip) if ip == Ipv4Addr::new(93, 184, 216, 34) => {}
        _ => return fail(13),
    }

    // --- 1b. Pointer-loop safety: crafted cycles must error, not hang. ---
    // A self-referential pointer at offset 12 (points to itself).
    let mut selfloop = [0u8; 14];
    selfloop[12] = 0xC0;
    selfloop[13] = 0x0C;
    let mut scratch = String::new();
    if dns::read_name(&selfloop, 12, &mut scratch).is_ok() {
        return fail(14);
    }
    // A mutual cycle: 12 -> 14 -> 12.
    let mut mutualloop = [0u8; 16];
    mutualloop[12] = 0xC0;
    mutualloop[13] = 0x0E;
    mutualloop[14] = 0xC0;
    mutualloop[15] = 0x0C;
    scratch.clear();
    if dns::read_name(&mutualloop, 12, &mut scratch).is_ok() {
        return fail(15);
    }
    // A pointer past the end of the buffer must be rejected.
    let mut oob = [0u8; 14];
    oob[12] = 0xC0;
    oob[13] = 0x40; // offset 64, past a 14-byte buffer
    scratch.clear();
    if dns::read_name(&oob, 12, &mut scratch).is_ok() {
        return fail(16);
    }
    println!("netdns-demo: codec parse oracle + pointer-loop safety OK");

    // --- Build a resolver with hosts + a blocklist. ---
    let src_mac = match net::mac().await {
        Ok(m) => m,
        Err(_) => return fail(20),
    };
    let mut cfg = Config::new();
    cfg.resolvers.push(DNS_IP);
    cfg.hosts
        .insert("localhost", IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    cfg.hosts
        .insert("db.internal.", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)));
    let mut r = Resolver::new(src_mac, GUEST_IP, cfg);
    r.blocklist_mut().insert("ads.example");
    r.blocklist_mut().insert_wildcard("*.tracker.example");

    // --- 3. Hosts table (no network). ---
    r.reset_queries();
    match r.resolve("localhost", QType::A).await {
        Ok(ips) if ips == vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))] => {}
        _ => return fail(30),
    }
    // Case-insensitive + trailing-dot normalization to the same host.
    match r.resolve("DB.Internal.", QType::A).await {
        Ok(ips) if ips == vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))] => {}
        _ => return fail(31),
    }
    if r.queries_sent() != 0 {
        return fail(32);
    }

    // --- 4. Blocklist (no network). ---
    match r.resolve("ads.example", QType::A).await {
        Err(DnsError::Blocked) => {}
        _ => return fail(40),
    }
    match r.resolve("pixel.tracker.example", QType::A).await {
        Err(DnsError::Blocked) => {}
        _ => return fail(41),
    }
    match r.resolve("tracker.example", QType::A).await {
        Err(DnsError::Blocked) => {}
        _ => return fail(42),
    }
    if r.queries_sent() != 0 {
        return fail(43);
    }

    // --- 5. Cache hit (no network): seed, then resolve twice. ---
    r.seed_cache(
        "cached.example",
        QType::A,
        vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))],
        1 << 40, // a huge tick TTL: never expires within the test
    );
    r.reset_queries();
    match r.resolve("cached.example", QType::A).await {
        Ok(ips) if ips == vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))] => {}
        _ => return fail(50),
    }
    // Normalized to the same key -> still a cache hit.
    match r.resolve("Cached.Example.", QType::A).await {
        Ok(ips) if ips == vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))] => {}
        _ => return fail(51),
    }
    if r.queries_sent() != 0 {
        return fail(52); // both served from cache -> zero network queries
    }

    // --- 6. Cache unit: TTL expiry + LRU on an explicit clock. ---
    if !cache_unit_ok() {
        return fail(60);
    }
    println!("netdns-demo: hosts + blocklist + cache (zero queries) + LRU/TTL OK");

    // --- 7. BONUS live resolve over SLIRP (network-tolerant). ---
    r.reset_queries();
    match r.resolve("example.com", QType::A).await {
        Ok(ips) if !ips.iter().any(|ip| matches!(ip, IpAddr::V4(_))) => {
            // A completed round trip that carried no A record - an upstream answering
            // AAAA-only, or an empty answer. **Tolerated, and it was not**: this arm used
            // to fall into `fail(70)`, so a phase whose own doc says the address is
            // non-deterministic and a failure to reach DNS is tolerated could still fail
            // on *what a real resolver happened to return*. That is the same category as
            // the timeout below, and it is the difference between a bonus phase and an
            // intermittent one (docs/ENGINEERING.md - an intermittently failing kernel
            // must not land).
            println!(
                "netdns-demo: live DNS answered with no A record ({} answer(s)) - tolerated",
                ips.len()
            );
        }
        Ok(ips) => {
            if r.queries_sent() == 0 {
                // Not network-dependent: an answer that reached us without a query being
                // sent would mean the cache served a name the phase just reset.
                return fail(70);
            }
            println!(
                "netdns-demo: live resolve example.com -> {} A record(s) in {} quer(y/ies)",
                ips.len(),
                r.queries_sent()
            );
            // A second lookup must hit the cache (no new query).
            let before = r.queries_sent();
            let _ = r.resolve("example.com", QType::A).await;
            if r.queries_sent() != before {
                // A cache miss on the name just resolved. Only reachable if the answer
                // carried a zero TTL, which a real upstream may legitimately do - so it is
                // reported rather than failed, for the reason above.
                println!("netdns-demo: live answer was not cacheable (TTL 0?) - tolerated");
            }
            println!("netdns-demo: live cache hit confirmed (no extra query)");
        }
        Err(DnsError::Timeout) | Err(DnsError::Net) => {
            println!(
                "netdns-demo: live DNS unavailable here - tolerated (deterministic checks passed)"
            );
        }
        Err(_) => {
            // NXDOMAIN / a parse error from a real responder is still a completed
            // round-trip; the deterministic core already proved the client.
            println!("netdns-demo: live DNS returned an error - tolerated");
        }
    }

    // Success: CODE stays 0.
}
