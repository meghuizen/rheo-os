//! `nethostcfg-demo` - the rheo-net Phase N4c proof cell (docs/NETSTACK.md §20):
//! **host configuration** - DHCP, zeroconf (IPv4 link-local + mDNS), NTP, and the
//! host-config store the rest of the stack reads. Its exit code is the proof.
//!
//! QEMU's SLIRP provides **no DHCP server on the emulated wire, no NTP server and
//! no mDNS peer**, so - exactly as `nettcp` and `nettrace` do - the real assertions
//! are **codec + state machine driven** and entirely network-free, with the live
//! attempts reported honestly as skips.
//!
//! ## What is asserted (each failure has its own exit code)
//!
//! **DHCP** (`10..59`)
//! 1. A **byte oracle** for an encoded DISCOVER: every field pinned at its wire
//!    offset (op/htype/hlen, xid, secs, the broadcast flag, `chaddr`, the magic
//!    cookie, option 53, the parameter-request list, END) *and* every byte not
//!    covered asserted zero, at the padded 300-byte length.
//! 2. The state-machine walk: DISCOVER -> (crafted OFFER) -> REQUEST -> (crafted
//!    ACK) -> BOUND, with the OFFER and ACK built by **our own encoder**
//!    ([`rheo_net::dhcp::build_reply`]) so encode and decode are both exercised on
//!    the same bytes. The REQUEST is decoded back and its `ciaddr` / requested-IP /
//!    server-id / type checked - the shape RFC 2131 §4.4.5 requires while selecting.
//! 3. A **decode oracle** on the ACK: `op`, `xid`, `chaddr`, `yiaddr`, `siaddr` and
//!    every option value byte-exactly.
//! 4. The extracted lease: address, mask, router, both DNS servers in order, the
//!    domain, and lease/T1/T2 - plus the **absolute T1/T2/expiry deadlines** the
//!    client armed.
//! 5. T1/T2 **defaults**: an ACK with no option 58/59 yields `lease/2` and
//!    `lease*7/8`, and a nonsensical `T1 > T2` pair is clamped back onto them.
//! 6. **Renewal**: nothing fires one nanosecond before T1; at T1 the client goes
//!    RENEWING and emits a **unicast** REQUEST to the leasing server with `ciaddr`
//!    set and the requested-IP/server-id options **absent**; a renewal ACK re-arms
//!    all three deadlines from the ACK's arrival.
//! 7. **Rebinding and expiry**: with no renewal ACK, T2 moves it to REBINDING with a
//!    **broadcast** REQUEST, and lease expiry drops the address, returns the client
//!    to SELECTING and emits a fresh DISCOVER with a **new** transaction id.
//! 8. **NAK** drops the lease and restarts.
//! 9. Seven malformed/hostile shapes each rejected with their **own** error: short,
//!    bad cookie, a truncated option, a BOOTREQUEST (not a reply), a foreign xid,
//!    another client's MAC, and a reply with no message-type option.
//! 10. **DECLINE** on a conflicting address and **RELEASE** of a held lease, each
//!     decoded back and checked.
//!
//! **hostcfg** (`60..69`) - an unconfigured store routes nowhere; the DHCP lease
//! populates it (address/mask/gateway/DNS/search domain/source); `prefix_len`,
//! `broadcast`, `netmask_is_valid`, on-link vs off-link `next_hop` and search-domain
//! `qualify` are all checked; and **two real stack paths read it back**: a
//! [`rheo_net::dns::Config`] whose resolvers are the leased DNS servers, and a
//! [`rheo_net::udp::UdpEndpoint`] whose source address is the leased address and
//! whose `next_hop` sends an off-link destination to the leased gateway.
//!
//! **zeroconf link-local** (`70..79`) - the candidate is a *usable* `169.254.x.y`
//! (outside the reserved first/last /24) and matches a **generator KAT**; the probe
//! frame decodes back to an ARP request with a **`0.0.0.0` sender**; a synthetic
//! conflicting ARP reply forces a **re-pick** to a different usable address and
//! resets probing; a probe from another MAC for the same candidate also conflicts,
//! while our own frame does not; the clean path sends 3 probes then 2 announcements
//! (sender == target == the claimed address) and reaches Claimed; and a
//! post-claim conflict is **defended once** (an explicit `defend()` frame, which does
//! not consume an announcement) then yields on a second conflict; and `announce()` is
//! **bounded** - it returns `None` once claimed, so a driver's drain loop terminates.
//!
//! **mDNS** (`80..89`) - byte oracles for an encoded `.local` query (id 0, no
//! recursion, class IN) and for the same query with the **QU** bit set; a crafted
//! response decoded through the **DNS codec** with the name, TTL, class and
//! **cache-flush** bit all checked; a non-flush response and a **goodbye** (TTL 0);
//! the RFC 1112 multicast-MAC mapping `224.0.0.251 -> 01:00:5e:00:00:fb`; and the
//! responder answering only its own `.local` name.
//!
//! **NTP** (`90..99`) - a byte oracle for the 48-byte request; a **hand-computed
//! known-answer test**: with T1/T2/T3/T4 = `S+0.0 / S+1.0 / S+1.5 / S+2.0` the
//! offset is exactly **+250 ms** and the delay exactly **1.5 s**, and the result is
//! a **bounded interval** whose half-width is exactly `delay/2` = **750 ms** (and,
//! with a root delay of 1 s and root dispersion of 0.5 s, exactly **1.75 s**), with
//! the true instant inside it and the endpoints exact; plus nine rejections (short,
//! client mode, bad version, Kiss-o'-Death, bad stratum, unsynchronized, zero
//! transmit, originate mismatch, non-monotonic timestamps) and the KoD **backoff**.
//!
//! **Live attempts** (never fatal) - a real DHCP DISCOVER, an NTP request to the
//! gateway, an mDNS query to `224.0.0.251` and a link-local ARP probe all go out over
//! the NIC. Each is bounded by a **duration**, never a drain count, and each reports
//! what actually happened: SLIRP **does** run a DHCP server, so that exchange is
//! normally answered and the lease is genuine (reported, not asserted - it is a
//! property of the QEMU backend); SLIRP runs no NTP service and hosts no mDNS peer, so
//! those two skip-with-reason after their windows elapse. A lease or a time sync is
//! **never** synthesised.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};

use rheo_net::arp::{self, ArpPacket};
use rheo_net::dhcp::{self, DhcpError, Event, Output, State, TimerEvent};
use rheo_net::dns::{self, QType};
use rheo_net::eth::{self, Mac};
use rheo_net::hostcfg::{ConfigSource, HostConfig};
use rheo_net::ip::{IpAddr, Ipv4Addr};
use rheo_net::ntp::{self, Estimate, NtpError, Timestamp};
use rheo_net::udp::UdpEndpoint;
use rheo_net::zeroconf::{self, ClaimState, LinkLocal, Observation, mdns};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test kernel asserts exactly this).
const OK_CODE: i32 = 0x42;

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

// ---------------------------------------------------------------- fixtures

/// The client MAC used throughout (QEMU's default virtio-net MAC shape).
const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
/// A fixed transaction id for the encoder oracles (so they are exact regardless of
/// the client's random id).
const ORACLE_XID: u32 = 0x1234_5678;

/// The address the crafted server hands out, and its network.
const LEASED: Ipv4Addr = Ipv4Addr::new(192, 168, 7, 42);
const MASK24: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const ROUTER: Ipv4Addr = Ipv4Addr::new(192, 168, 7, 1);
const DNS1: Ipv4Addr = Ipv4Addr::new(192, 168, 7, 53);
const DNS2: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const SERVER_ID: Ipv4Addr = Ipv4Addr::new(192, 168, 7, 1);
const DOMAIN: &str = "lan.example";
const LEASE_SECS: u32 = 3600;
const T1_SECS: u32 = 1800;
const T2_SECS: u32 = 3150;

const NS: u64 = 1_000_000_000;

/// SLIRP's gateway (the live NTP attempt's target) - read from the host-config
/// store's SLIRP profile rather than written inline.
fn slirp_gateway() -> Ipv4Addr {
    HostConfig::slirp()
        .gateway()
        .unwrap_or(Ipv4Addr::new(10, 0, 2, 2))
}

/// A span of an encoded message pinned to its wire offset.
type Span = (usize, &'static [u8]);

/// Verify a **byte oracle**: `buf` must be exactly `total` bytes, every `span` must
/// match at its offset, and **every byte not covered by a span must be zero**. That
/// last clause is what makes this a complete oracle rather than a spot check - a
/// stray option appended anywhere would fail it.
fn oracle_ok(buf: &[u8], spans: &[Span], total: usize) -> bool {
    if buf.len() != total {
        return false;
    }
    let mut covered = alloc::vec![false; total];
    for (off, bytes) in spans {
        if off + bytes.len() > total {
            return false;
        }
        if &buf[*off..*off + bytes.len()] != *bytes {
            return false;
        }
        for c in covered[*off..*off + bytes.len()].iter_mut() {
            *c = true;
        }
    }
    for (i, &c) in covered.iter().enumerate() {
        if !c && buf[i] != 0 {
            return false;
        }
    }
    true
}

/// The DISCOVER byte oracle: every significant field at its wire offset. Anything
/// not listed must be zero (see [`oracle_ok`]).
const DISCOVER_SPANS: &[Span] = &[
    // op = BOOTREQUEST, htype = Ethernet, hlen = 6, hops = 0.
    (0, &[1, 1, 6, 0]),
    // xid.
    (4, &[0x12, 0x34, 0x56, 0x78]),
    // secs = 0 is covered by the all-zero rule; flags = broadcast.
    (10, &[0x80, 0x00]),
    // chaddr: our MAC in the first 6 of 16 bytes.
    (28, &[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
    // the magic cookie at 236.
    (236, &[0x63, 0x82, 0x53, 0x63]),
    // option 53 (message type) = 1 (DISCOVER).
    (240, &[53, 1, 1]),
    // option 55 (parameter request list): mask, router, DNS, domain.
    (243, &[55, 4, 1, 3, 6, 15]),
    // END.
    (249, &[255]),
];

/// Build the crafted server reply the client is fed.
fn reply(msg_type: u8, xid: u32, with_t1t2: bool) -> Vec<u8> {
    let mut p = dhcp::ReplyParams::new(msg_type, xid, MAC, LEASED, SERVER_ID);
    p.netmask = Some(MASK24);
    p.router = Some(ROUTER);
    p.dns = alloc::vec![DNS1, DNS2];
    p.domain = Some(String::from(DOMAIN));
    p.lease_secs = Some(LEASE_SECS);
    if with_t1t2 {
        p.t1_secs = Some(T1_SECS);
        p.t2_secs = Some(T2_SECS);
    }
    dhcp::build_reply(&p)
}

// ---------------------------------------------------------------- 1. DHCP

/// The full DHCP proof. Returns the lease so hostcfg can be checked against it.
fn dhcp_ok() -> Result<dhcp::Lease, i32> {
    // --- 1. The DISCOVER byte oracle (encode). ---
    let discover = dhcp::build_discover(ORACLE_XID, MAC, 0, None, None);
    if !oracle_ok(&discover, DISCOVER_SPANS, dhcp::PADDED_LEN) {
        return Err(10);
    }
    // It must also decode back through our own parser (round trip).
    let m = dhcp::parse(&discover).map_err(|_| 11)?;
    if m.op != dhcp::BOOTREQUEST
        || m.xid != ORACLE_XID
        || m.chaddr != MAC
        || m.flags != dhcp::FLAG_BROADCAST
        || m.ciaddr != Ipv4Addr::new(0, 0, 0, 0)
    {
        return Err(12);
    }
    if m.msg_type() != Ok(dhcp::msg::DISCOVER) {
        return Err(13);
    }
    if m.options.get(dhcp::opt::PARAM_REQUEST_LIST) != Some(&dhcp::PARAM_REQUESTS[..]) {
        return Err(14);
    }

    // --- 2. The state-machine walk. ---
    let mut c = dhcp::Client::new(MAC, 0x1234);
    if c.state() != State::Init {
        return Err(20);
    }
    let out = c.start(0);
    if c.state() != State::Selecting {
        return Err(21);
    }
    // The client's DISCOVER is a broadcast carrying its own xid.
    let Output::Broadcast(bytes) = &out else {
        return Err(22);
    };
    let sent = dhcp::parse(bytes).map_err(|_| 23)?;
    if sent.xid != c.xid() || sent.msg_type() != Ok(dhcp::msg::DISCOVER) {
        return Err(24);
    }

    // Feed a crafted OFFER built with our own encoder.
    let offer = reply(dhcp::msg::OFFER, c.xid(), true);
    let (ev, out) = c.on_message(&offer, 0).map_err(|_| 25)?;
    if ev != Event::Offered(LEASED) || c.state() != State::Requesting {
        return Err(26);
    }
    // The REQUEST while selecting: broadcast, ciaddr zero, requested-IP AND
    // server-id present (RFC 2131 §4.4.5).
    let Some(Output::Broadcast(req)) = out else {
        return Err(27);
    };
    let rq = dhcp::parse(&req).map_err(|_| 28)?;
    if rq.msg_type() != Ok(dhcp::msg::REQUEST)
        || rq.ciaddr != Ipv4Addr::new(0, 0, 0, 0)
        || rq.flags != dhcp::FLAG_BROADCAST
        || rq.options.get_ipv4(dhcp::opt::REQUESTED_IP) != Some(LEASED)
        || rq.options.get_ipv4(dhcp::opt::SERVER_ID) != Some(SERVER_ID)
    {
        return Err(29);
    }

    // --- 3. The ACK decode oracle. ---
    let ack = reply(dhcp::msg::ACK, c.xid(), true);
    let am = dhcp::parse(&ack).map_err(|_| 30)?;
    if am.op != dhcp::BOOTREPLY
        || am.xid != c.xid()
        || am.chaddr != MAC
        || am.yiaddr != LEASED
        || am.siaddr != SERVER_ID
    {
        return Err(31);
    }
    if am.options.get_u8(dhcp::opt::MSG_TYPE) != Some(dhcp::msg::ACK)
        || am.options.get(dhcp::opt::SUBNET_MASK) != Some(&[255, 255, 255, 0][..])
        || am.options.get(dhcp::opt::ROUTER) != Some(&[192, 168, 7, 1][..])
        || am.options.get(dhcp::opt::DNS_SERVER) != Some(&[192, 168, 7, 53, 8, 8, 8, 8][..])
        || am.options.get(dhcp::opt::DOMAIN_NAME) != Some(DOMAIN.as_bytes())
        || am.options.get_u32(dhcp::opt::LEASE_TIME) != Some(LEASE_SECS)
        || am.options.get_u32(dhcp::opt::RENEWAL_T1) != Some(T1_SECS)
        || am.options.get_u32(dhcp::opt::REBINDING_T2) != Some(T2_SECS)
        || am.options.get_ipv4(dhcp::opt::SERVER_ID) != Some(SERVER_ID)
    {
        return Err(32);
    }

    // --- 4. BOUND, with the lease and the armed deadlines exact. ---
    let (ev, out) = c.on_message(&ack, 0).map_err(|_| 33)?;
    if ev != Event::Bound || out.is_some() || c.state() != State::Bound {
        return Err(34);
    }
    let lease = c.lease().ok_or(35)?.clone();
    if lease.address != LEASED
        || lease.netmask != Some(MASK24)
        || lease.router != Some(ROUTER)
        || lease.dns != alloc::vec![DNS1, DNS2]
        || lease.domain.as_deref() != Some(DOMAIN)
        || lease.server_id != SERVER_ID
        || lease.lease_secs != LEASE_SECS
        || lease.t1_secs != T1_SECS
        || lease.t2_secs != T2_SECS
    {
        return Err(36);
    }
    if c.t1_ns() != T1_SECS as u64 * NS
        || c.t2_ns() != T2_SECS as u64 * NS
        || c.expires_ns() != LEASE_SECS as u64 * NS
    {
        return Err(37);
    }
    if c.next_deadline_ns() != Some(T1_SECS as u64 * NS) {
        return Err(38);
    }

    // --- 5. T1/T2 defaults and clamping. ---
    {
        let mut d = dhcp::Client::new(MAC, 7);
        let _ = d.start(0);
        let offer = reply(dhcp::msg::OFFER, d.xid(), false);
        let _ = d.on_message(&offer, 0).map_err(|_| 40)?;
        // A 1200 s lease with no T1/T2: 600 and 1050 (7/8).
        let mut p = dhcp::ReplyParams::new(dhcp::msg::ACK, d.xid(), MAC, LEASED, SERVER_ID);
        p.lease_secs = Some(1200);
        let ack = dhcp::build_reply(&p);
        let (ev, _) = d.on_message(&ack, 0).map_err(|_| 41)?;
        if ev != Event::Bound {
            return Err(42);
        }
        let l = d.lease().ok_or(43)?;
        if l.t1_secs != 600 || l.t2_secs != 1050 {
            return Err(44);
        }
    }
    {
        // T1 > T2 is nonsense; it must be clamped back onto the defaults rather
        // than making the client skip RENEWING.
        let mut d = dhcp::Client::new(MAC, 9);
        let _ = d.start(0);
        let offer = reply(dhcp::msg::OFFER, d.xid(), false);
        let _ = d.on_message(&offer, 0).map_err(|_| 45)?;
        let mut p = dhcp::ReplyParams::new(dhcp::msg::ACK, d.xid(), MAC, LEASED, SERVER_ID);
        p.lease_secs = Some(800);
        p.t1_secs = Some(700);
        p.t2_secs = Some(100);
        let ack = dhcp::build_reply(&p);
        let _ = d.on_message(&ack, 0).map_err(|_| 46)?;
        let l = d.lease().ok_or(47)?;
        if l.t1_secs >= l.t2_secs || l.t2_secs > 800 {
            return Err(48);
        }
    }

    // --- 6. Renewal at T1. ---
    // One nanosecond early: nothing fires.
    if c.poll_timers(T1_SECS as u64 * NS - 1).is_some() {
        return Err(50);
    }
    let (te, out) = c.poll_timers(T1_SECS as u64 * NS).ok_or(51)?;
    if te != TimerEvent::Renewing || c.state() != State::Renewing {
        return Err(52);
    }
    // RENEWING: unicast to the leasing server, ciaddr set, requested-IP and
    // server-id ABSENT (RFC 2131 §4.4.5 table 5).
    let Output::Unicast { to, data } = &out else {
        return Err(53);
    };
    if *to != SERVER_ID {
        return Err(54);
    }
    let rn = dhcp::parse(data).map_err(|_| 55)?;
    if rn.msg_type() != Ok(dhcp::msg::REQUEST)
        || rn.ciaddr != LEASED
        || rn.flags != 0
        || rn.options.get(dhcp::opt::REQUESTED_IP).is_some()
        || rn.options.get(dhcp::opt::SERVER_ID).is_some()
    {
        return Err(56);
    }
    // A renewal ACK arriving at T1 re-arms all three deadlines from *then*.
    let renew_at = T1_SECS as u64 * NS;
    let ack2 = reply(dhcp::msg::ACK, c.xid(), true);
    let (ev, _) = c.on_message(&ack2, renew_at).map_err(|_| 57)?;
    if ev != Event::Renewed || c.state() != State::Bound {
        return Err(58);
    }
    if c.t1_ns() != renew_at + T1_SECS as u64 * NS
        || c.expires_ns() != renew_at + LEASE_SECS as u64 * NS
    {
        return Err(59);
    }

    // --- 7. Rebinding then expiry, on a fresh client. ---
    let mut e = dhcp::Client::new(MAC, 0xABCD);
    let _ = e.start(0);
    let offer = reply(dhcp::msg::OFFER, e.xid(), true);
    let _ = e.on_message(&offer, 0).map_err(|_| 60)?;
    let ack = reply(dhcp::msg::ACK, e.xid(), true);
    let _ = e.on_message(&ack, 0).map_err(|_| 61)?;
    let (te, _) = e.poll_timers(T1_SECS as u64 * NS).ok_or(62)?;
    if te != TimerEvent::Renewing {
        return Err(63);
    }
    // No ACK: T2 moves it to REBINDING with a broadcast.
    let (te, out) = e.poll_timers(T2_SECS as u64 * NS).ok_or(64)?;
    if te != TimerEvent::Rebinding || e.state() != State::Rebinding {
        return Err(65);
    }
    let Output::Broadcast(rb) = &out else {
        return Err(66);
    };
    let rbm = dhcp::parse(rb).map_err(|_| 67)?;
    if rbm.msg_type() != Ok(dhcp::msg::REQUEST) || rbm.ciaddr != LEASED {
        return Err(68);
    }
    // Still no ACK: the lease expires, the address is dropped, and a *new*
    // transaction begins.
    let old_xid = e.xid();
    let (te, out) = e.poll_timers(LEASE_SECS as u64 * NS).ok_or(69)?;
    if te != TimerEvent::Expired || e.state() != State::Selecting || e.lease().is_some() {
        return Err(70);
    }
    if e.xid() == old_xid {
        return Err(71); // expiry must start a fresh transaction
    }
    let Output::Broadcast(again) = &out else {
        return Err(72);
    };
    if dhcp::parse(again).map_err(|_| 73)?.msg_type() != Ok(dhcp::msg::DISCOVER) {
        return Err(74);
    }

    // --- 8. NAK drops the lease and restarts. ---
    {
        let mut n = dhcp::Client::new(MAC, 0x55);
        let _ = n.start(0);
        let offer = reply(dhcp::msg::OFFER, n.xid(), true);
        let _ = n.on_message(&offer, 0).map_err(|_| 75)?;
        let mut p = dhcp::ReplyParams::new(
            dhcp::msg::NAK,
            n.xid(),
            MAC,
            Ipv4Addr::new(0, 0, 0, 0),
            SERVER_ID,
        );
        p.message = Some(String::from("lease gone"));
        let nak = dhcp::build_reply(&p);
        let (ev, out) = n.on_message(&nak, 0).map_err(|_| 76)?;
        if ev != Event::Nak || n.lease().is_some() || n.state() != State::Selecting {
            return Err(77);
        }
        let Some(Output::Broadcast(d)) = out else {
            return Err(78);
        };
        if dhcp::parse(&d).map_err(|_| 79)?.msg_type() != Ok(dhcp::msg::DISCOVER) {
            return Err(80);
        }
    }

    // --- 9. Malformed / hostile shapes, each with its own error. ---
    {
        let mut r = dhcp::Client::new(MAC, 0x77);
        let _ = r.start(0);
        let good = reply(dhcp::msg::OFFER, r.xid(), true);

        // Too short (one byte below the fixed part + cookie).
        if r.on_message(&good[..dhcp::MIN_LEN - 1], 0) != Err(DhcpError::TooShort) {
            return Err(81);
        }
        // A corrupted magic cookie is BOOTP, not DHCP.
        let mut bad = good.clone();
        bad[dhcp::FIXED_LEN] ^= 0xFF;
        if r.on_message(&bad, 0) != Err(DhcpError::BadCookie) {
            return Err(82);
        }
        // An option whose length runs past the end of the message.
        let mut trunc = good[..dhcp::MIN_LEN].to_vec();
        trunc.push(dhcp::opt::DNS_SERVER);
        trunc.push(8); // claims 8 bytes, supplies none
        if r.on_message(&trunc, 0) != Err(DhcpError::TruncatedOption) {
            return Err(83);
        }
        // A client request is not a reply.
        let mut notreply = good.clone();
        notreply[0] = dhcp::BOOTREQUEST;
        if r.on_message(&notreply, 0) != Err(DhcpError::NotAReply) {
            return Err(84);
        }
        // Another client's transaction.
        let foreign = reply(dhcp::msg::OFFER, r.xid() ^ 0xFFFF_FFFF, true);
        if r.on_message(&foreign, 0) != Err(DhcpError::XidMismatch) {
            return Err(85);
        }
        // Another client's MAC.
        let mut othermac = good.clone();
        othermac[28] ^= 0xFF;
        if r.on_message(&othermac, 0) != Err(DhcpError::NotOurMac) {
            return Err(86);
        }
        // A reply with no message-type option at all.
        let mut p = dhcp::ReplyParams::new(dhcp::msg::ACK, r.xid(), MAC, LEASED, SERVER_ID);
        p.lease_secs = Some(60);
        let mut notype = dhcp::build_reply(&p);
        // Overwrite the type option's code with PAD, leaving its TLV bytes as junk
        // the walker will read as options - the point is only that 53 is gone.
        notype[dhcp::MIN_LEN] = dhcp::opt::END;
        for b in notype[dhcp::MIN_LEN + 1..].iter_mut() {
            *b = dhcp::opt::PAD;
        }
        if r.on_message(&notype, 0) != Err(DhcpError::NoMessageType) {
            return Err(87);
        }
        // The state machine must have survived all of that untouched.
        if r.state() != State::Selecting || r.lease().is_some() {
            return Err(88);
        }
    }

    // --- 10. DECLINE and RELEASE. ---
    {
        // DECLINE: the offered address turned out to be in use.
        let mut d = dhcp::Client::new(MAC, 0x99);
        let _ = d.start(0);
        let offer = reply(dhcp::msg::OFFER, d.xid(), true);
        let _ = d.on_message(&offer, 0).map_err(|_| 89)?;
        let out = d.decline(0).map_err(|_| 90)?;
        let Output::Broadcast(dec) = &out else {
            return Err(91);
        };
        let dm = dhcp::parse(dec).map_err(|_| 92)?;
        if dm.msg_type() != Ok(dhcp::msg::DECLINE)
            || dm.options.get_ipv4(dhcp::opt::REQUESTED_IP) != Some(LEASED)
            || dm.options.get_ipv4(dhcp::opt::SERVER_ID) != Some(SERVER_ID)
        {
            return Err(93);
        }
        if d.state() != State::Init {
            return Err(94);
        }
        // Declining with nothing to decline is refused.
        if d.decline(0) != Err(DhcpError::NoLease) {
            return Err(95);
        }
    }
    {
        // RELEASE: hand a held lease back.
        let mut d = dhcp::Client::new(MAC, 0xAA);
        let _ = d.start(0);
        let offer = reply(dhcp::msg::OFFER, d.xid(), true);
        let _ = d.on_message(&offer, 0).map_err(|_| 96)?;
        let ack = reply(dhcp::msg::ACK, d.xid(), true);
        let _ = d.on_message(&ack, 0).map_err(|_| 97)?;
        let out = d.release().map_err(|_| 98)?;
        let Output::Unicast { to, data } = &out else {
            return Err(99);
        };
        if *to != SERVER_ID {
            return Err(100);
        }
        let rm = dhcp::parse(data).map_err(|_| 101)?;
        if rm.msg_type() != Ok(dhcp::msg::RELEASE) || rm.ciaddr != LEASED {
            return Err(102);
        }
        if d.lease().is_some() || d.state() != State::Init {
            return Err(103);
        }
        if d.release() != Err(DhcpError::NoLease) {
            return Err(104);
        }
    }

    Ok(lease)
}

// ------------------------------------------------------------- 2. hostcfg

fn hostcfg_ok(lease: &dhcp::Lease, mac: Mac) -> Result<(), i32> {
    // An unconfigured store routes nowhere and claims nothing.
    let empty = HostConfig::new();
    if empty.is_configured()
        || empty.source() != ConfigSource::Unconfigured
        || empty.prefix_len().is_some()
        || empty.netmask_is_valid()
        || empty.broadcast().is_some()
        || empty.is_on_link(LEASED)
        || empty.next_hop(Ipv4Addr::new(1, 1, 1, 1)).is_some()
    {
        return Err(110);
    }
    // The unconfigured source address is 0.0.0.0 - what a DHCP client must send from.
    if empty.source_address() != Ipv4Addr::new(0, 0, 0, 0) {
        return Err(111);
    }

    // The lease populates it.
    let mut cfg = HostConfig::new();
    cfg.apply_lease(lease);
    if cfg.source() != ConfigSource::Dhcp
        || cfg.address() != Some(LEASED)
        || cfg.netmask() != Some(MASK24)
        || cfg.gateway() != Some(ROUTER)
        || cfg.dns_servers() != [DNS1, DNS2]
        || cfg.search_domains() != [String::from(DOMAIN)]
        || cfg.lease_secs() != Some(LEASE_SECS)
    {
        return Err(112);
    }
    if cfg.prefix_len() != Some(24) || !cfg.netmask_is_valid() {
        return Err(113);
    }
    if cfg.broadcast() != Some(Ipv4Addr::new(192, 168, 7, 255)) {
        return Err(114);
    }

    // The routing decision.
    let onlink = Ipv4Addr::new(192, 168, 7, 99);
    let offlink = Ipv4Addr::new(93, 184, 216, 34);
    if !cfg.is_on_link(onlink) || cfg.is_on_link(offlink) {
        return Err(115);
    }
    if cfg.next_hop(onlink) != Some(onlink) || cfg.next_hop(offlink) != Some(ROUTER) {
        return Err(116);
    }
    // The all-ones broadcast is never routed through a gateway.
    if cfg.next_hop(Ipv4Addr::new(255, 255, 255, 255)) != Some(Ipv4Addr::new(255, 255, 255, 255)) {
        return Err(117);
    }

    // Search-domain expansion.
    cfg.set_hostname("node7");
    let q = cfg.qualify("printer");
    if q != alloc::vec![String::from("printer.lan.example"), String::from("printer")] {
        return Err(118);
    }
    // A name that already has a dot, or is explicitly absolute, is left alone.
    if cfg.qualify("www.example.com") != alloc::vec![String::from("www.example.com")]
        || cfg.qualify("printer.") != alloc::vec![String::from("printer")]
    {
        return Err(119);
    }

    // --- The stack reads it back: the DNS resolver config. ---
    let dcfg = cfg.dns_config();
    if dcfg.resolvers != alloc::vec![DNS1, DNS2] {
        return Err(120);
    }
    // The hostname resolves to our own address with no query at all.
    if dcfg.hosts.lookup("node7", QType::A) != Some(alloc::vec![IpAddr::V4(LEASED)]) {
        return Err(121);
    }

    // --- The stack reads it back: a UDP endpoint's source address + routing. ---
    let ep = UdpEndpoint::from_host_config(mac, &cfg);
    if ep.src_ip() != LEASED || ep.gateway() != Some(ROUTER) {
        return Err(122);
    }
    if ep.next_hop(onlink) != onlink || ep.next_hop(offlink) != ROUTER {
        return Err(123);
    }
    // An endpoint built the old way has no netmask, so nothing is routed - the
    // pre-N4c behaviour, unchanged.
    let plain = UdpEndpoint::new(mac, LEASED);
    if plain.next_hop(offlink) != offlink || plain.gateway().is_some() {
        return Err(124);
    }

    // A link-local claim overwrites the address AND clears the gateway - a
    // link-local host has no route off the link.
    let ll = Ipv4Addr::new(169, 254, 7, 7);
    cfg.apply_link_local(ll);
    if cfg.source() != ConfigSource::LinkLocal
        || cfg.address() != Some(ll)
        || cfg.netmask() != Some(Ipv4Addr::new(255, 255, 0, 0))
        || cfg.gateway().is_some()
        || cfg.prefix_len() != Some(16)
    {
        return Err(125);
    }
    if cfg.next_hop(offlink).is_some() {
        return Err(126); // off-link with no gateway must fail, not guess
    }
    if cfg.next_hop(Ipv4Addr::new(169, 254, 1, 1)) != Some(Ipv4Addr::new(169, 254, 1, 1)) {
        return Err(127);
    }

    // The static SLIRP profile is the one place those addresses are named.
    let s = HostConfig::slirp();
    if s.source() != ConfigSource::Static
        || s.address() != Some(Ipv4Addr::new(10, 0, 2, 15))
        || s.gateway() != Some(Ipv4Addr::new(10, 0, 2, 2))
        || s.dns_servers() != [Ipv4Addr::new(10, 0, 2, 3)]
    {
        return Err(128);
    }

    // Clearing (what expiry does) returns it to unconfigured.
    cfg.clear();
    if cfg.is_configured() || cfg.source() != ConfigSource::Unconfigured {
        return Err(129);
    }
    Ok(())
}

// -------------------------------------------------- 3. zeroconf link-local

/// The candidate `LinkLocal::new(MAC, 0xC0FFEE)` must draw, and the one it must
/// draw after a conflict. A **generator KAT**: it pins splitmix64, the MAC mixing,
/// and the `1..=254` third-octet mapping all at once, so a change to any of them is
/// caught rather than silently producing "some other valid address".
const LL_FIRST: Ipv4Addr = Ipv4Addr::new(169, 254, 109, 114);
const LL_SECOND: Ipv4Addr = Ipv4Addr::new(169, 254, 97, 106);

/// Decode a built ARP frame back into its Ethernet header + ARP packet, so the
/// proof reads what actually went on the wire rather than trusting the builder.
fn decode_arp(frame: &[u8]) -> Option<(Mac, ArpPacket)> {
    let f = eth::Frame::parse(frame)?;
    if f.ethertype() != eth::ethertype::ARP {
        return None;
    }
    Some((f.dst(), ArpPacket::parse(f.payload())?))
}

fn linklocal_ok() -> Result<(), i32> {
    let mac = Mac(MAC);
    let mut ll = LinkLocal::new(mac, 0xC0FFEE);

    // The candidate is usable and matches the generator KAT.
    if !zeroconf::is_usable_link_local(ll.address()) {
        return Err(140);
    }
    if ll.address() != LL_FIRST {
        return Err(141);
    }
    if ll.state() != ClaimState::Probing {
        return Err(142);
    }

    // The probe frame: broadcast, ARP request, sender address 0.0.0.0, target the
    // candidate. That zero sender is the whole point of a probe.
    let frame = ll.probe().ok_or(143)?;
    let (dst, pkt) = decode_arp(&frame).ok_or(144)?;
    if dst != eth::BROADCAST
        || pkt.oper != arp::OP_REQUEST
        || pkt.spa != Ipv4Addr::new(0, 0, 0, 0)
        || pkt.tpa != LL_FIRST
        || pkt.sha != mac
    {
        return Err(145);
    }

    // Our own frame coming back is not a conflict.
    if ll.observe(&pkt, 0) != Observation::Unrelated {
        return Err(146);
    }
    // An unrelated host's ARP is not a conflict either.
    let other = Mac([0x02, 0, 0, 0, 0, 1]);
    let unrelated = ArpPacket {
        oper: arp::OP_REPLY,
        sha: other,
        spa: Ipv4Addr::new(169, 254, 200, 200),
        tha: mac,
        tpa: LL_FIRST,
    };
    if ll.observe(&unrelated, 0) != Observation::Unrelated {
        return Err(147);
    }

    // A synthetic **conflicting** ARP reply: somebody answers for our candidate.
    let conflict = ArpPacket {
        oper: arp::OP_REPLY,
        sha: other,
        spa: LL_FIRST,
        tha: mac,
        tpa: LL_FIRST,
    };
    if ll.observe(&conflict, 0) != Observation::Conflict {
        return Err(148);
    }
    // It re-picked: a different, still-usable address, and probing restarted.
    if ll.address() == LL_FIRST || !zeroconf::is_usable_link_local(ll.address()) {
        return Err(149);
    }
    if ll.address() != LL_SECOND {
        return Err(150);
    }
    if ll.state() != ClaimState::Probing || ll.probes_sent() != 0 || ll.conflicts() != 1 {
        return Err(151);
    }

    // Another host **probing** the same candidate (sender 0.0.0.0, different MAC) is
    // also a conflict - it is picking the address right now.
    let racing = ArpPacket {
        oper: arp::OP_REQUEST,
        sha: other,
        spa: Ipv4Addr::new(0, 0, 0, 0),
        tha: Mac([0; 6]),
        tpa: ll.address(),
    };
    let before = ll.address();
    if ll.observe(&racing, 0) != Observation::Conflict {
        return Err(152);
    }
    if ll.address() == before || ll.conflicts() != 2 {
        return Err(153);
    }

    // The clean path: 3 probes, no conflict, then 2 announcements -> Claimed.
    let claimed = ll.address();
    for i in 0..zeroconf::PROBE_COUNT {
        if ll.probe().is_none() {
            return Err(154);
        }
        if ll.probes_sent() != i + 1 {
            return Err(155);
        }
    }
    // A fourth probe is refused and moves the machine on to announcing.
    if ll.probe().is_some() || ll.state() != ClaimState::Announcing {
        return Err(156);
    }
    for _ in 0..zeroconf::ANNOUNCE_COUNT {
        let frame = ll.announce().ok_or(157)?;
        let (dst, pkt) = decode_arp(&frame).ok_or(158)?;
        // An announcement claims the address: sender AND target are it.
        if dst != eth::BROADCAST
            || pkt.oper != arp::OP_REQUEST
            || pkt.spa != claimed
            || pkt.tpa != claimed
            || pkt.sha != mac
        {
            return Err(159);
        }
    }
    if ll.state() != ClaimState::Claimed || ll.address() != claimed {
        return Err(160);
    }
    // **Announcing is bounded.** Once claimed, `announce` returns None for good, so a
    // driver's `while let Some(f) = ll.announce()` terminates. Defending is a
    // different act with its own method - folding the two together made `announce`
    // return a frame forever and hung the claim driver.
    if ll.announce().is_some() || ll.announces_sent() != zeroconf::ANNOUNCE_COUNT {
        return Err(165);
    }

    // A conflict after claiming: defend once...
    let steal = ArpPacket {
        oper: arp::OP_REPLY,
        sha: other,
        spa: claimed,
        tha: mac,
        tpa: claimed,
    };
    if ll.observe(&steal, 1_000) != Observation::Defend {
        return Err(161);
    }
    if ll.address() != claimed || ll.state() != ClaimState::Claimed {
        return Err(162); // defending does not give up the address
    }
    // The defence frame is the announcement re-sent, and it does not consume an
    // announcement from the RFC 3927 budget.
    let frame = ll.defend().ok_or(166)?;
    let (dst, pkt) = decode_arp(&frame).ok_or(167)?;
    if dst != eth::BROADCAST
        || pkt.oper != arp::OP_REQUEST
        || pkt.spa != claimed
        || pkt.tpa != claimed
        || ll.announces_sent() != zeroconf::ANNOUNCE_COUNT
    {
        return Err(168);
    }
    // ...and yield on a second conflict inside the defend window.
    if ll.observe(&steal, 2_000) != Observation::Conflict {
        return Err(163);
    }
    if ll.address() == claimed || ll.state() != ClaimState::Probing {
        return Err(164);
    }
    Ok(())
}

// -------------------------------------------------------------- 4. mDNS

/// A `.local` mDNS query for `printer.local A`, class IN, **no** QU bit: id 0 and
/// flags 0 are what distinguish it from a unicast DNS query.
const MDNS_QUERY: &[u8] = &[
    0x00, 0x00, // id 0 - mDNS does not correlate by id
    0x00, 0x00, // flags 0 - no recursion desired
    0x00, 0x01, // qdcount 1
    0x00, 0x00, // ancount 0
    0x00, 0x00, // nscount 0
    0x00, 0x00, // arcount 0
    0x07, b'p', b'r', b'i', b'n', b't', b'e', b'r', //
    0x05, b'l', b'o', b'c', b'a', b'l', //
    0x00, // root label
    0x00, 0x01, // qtype A
    0x00, 0x01, // qclass IN, QU clear
];

/// The same query with the **QU** bit set - "answer me by unicast".
const MDNS_QUERY_QU: &[u8] = &[
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x07, b'p', b'r', b'i', b'n', b't', b'e', b'r', //
    0x05, b'l', b'o', b'c', b'a', b'l', //
    0x00, //
    0x00, 0x01, // qtype A
    0x80, 0x01, // qclass IN with the QU bit
];

/// A crafted mDNS **response**: authoritative, one A record for `printer.local` ->
/// `169.254.7.7`, TTL 120, with the **cache-flush** bit set in the class.
const MDNS_RESPONSE: &[u8] = &[
    0x00, 0x00, // id 0
    0x84, 0x00, // flags: response + authoritative
    0x00, 0x00, // qdcount 0 (an unsolicited announcement)
    0x00, 0x01, // ancount 1
    0x00, 0x00, 0x00, 0x00, //
    0x07, b'p', b'r', b'i', b'n', b't', b'e', b'r', //
    0x05, b'l', b'o', b'c', b'a', b'l', //
    0x00, //
    0x00, 0x01, // type A
    0x80, 0x01, // class IN with the cache-flush bit
    0x00, 0x00, 0x00, 0x78, // ttl 120
    0x00, 0x04, // rdlength 4
    169, 254, 7, 7, // rdata
];

const MDNS_NAME: &str = "printer.local";
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 7, 7);

fn mdns_ok() -> Result<(), i32> {
    // --- Encode oracles. ---
    let q = mdns::build_query(MDNS_NAME, QType::A, false).ok_or(170)?;
    if q != MDNS_QUERY {
        return Err(171);
    }
    let qu = mdns::build_query(MDNS_NAME, QType::A, true).ok_or(172)?;
    if qu != MDNS_QUERY_QU {
        return Err(173);
    }
    let r = mdns::build_response(MDNS_NAME, MDNS_ADDR, mdns::TTL_HOSTNAME, true).ok_or(174)?;
    if r != MDNS_RESPONSE {
        return Err(175);
    }

    // --- Decode through the DNS codec (the point: one codec, two protocols). ---
    let questions = mdns::parse_query(MDNS_QUERY).map_err(|_| 176)?;
    if questions.len() != 1 {
        return Err(177);
    }
    let qn = &questions[0];
    if qn.name != MDNS_NAME
        || qn.qtype != dns::TYPE_A
        || qn.class() != dns::CLASS_IN
        || qn.unicast_response()
    {
        return Err(178);
    }
    let qu_questions = mdns::parse_query(MDNS_QUERY_QU).map_err(|_| 179)?;
    // The QU bit must be visible AND must not corrupt the class.
    if !qu_questions[0].unicast_response() || qu_questions[0].class() != dns::CLASS_IN {
        return Err(180);
    }

    let records = mdns::parse_response(MDNS_RESPONSE).map_err(|_| 181)?;
    if records.len() != 1 {
        return Err(182);
    }
    let rec = &records[0];
    if rec.name != MDNS_NAME
        || rec.rtype != dns::TYPE_A
        || rec.ttl != 120
        || !rec.cache_flush()
        || rec.class() != dns::CLASS_IN
        || rec.is_goodbye()
    {
        return Err(183);
    }
    if rec.data != dns::RData::A(MDNS_ADDR) {
        return Err(184);
    }

    // A response WITHOUT the cache-flush bit must read as a merge, not a flush.
    let plain = mdns::build_response(MDNS_NAME, MDNS_ADDR, 120, false).ok_or(185)?;
    let pr = mdns::parse_response(&plain).map_err(|_| 186)?;
    if pr[0].cache_flush() || pr[0].class() != dns::CLASS_IN {
        return Err(187);
    }

    // A **goodbye** is TTL 0 - and the cache-flush bit is still set.
    let bye = mdns::build_goodbye(MDNS_NAME, MDNS_ADDR).ok_or(188)?;
    let br = mdns::parse_response(&bye).map_err(|_| 189)?;
    if !br[0].is_goodbye() || br[0].ttl != 0 || !br[0].cache_flush() {
        return Err(190);
    }

    // --- The multicast MAC mapping (RFC 1112 §6.4 known answer). ---
    if mdns::multicast_mac(mdns::GROUP) != Mac([0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb]) {
        return Err(191);
    }
    // The top bit of the second octet is dropped - 224.x and 225.x share a MAC,
    // which is why a receiver must still check the IP address.
    if mdns::multicast_mac(Ipv4Addr::new(239, 128, 1, 2))
        != Mac([0x01, 0x00, 0x5e, 0x00, 0x01, 0x02])
    {
        return Err(192);
    }

    // --- `.local` scoping. ---
    if !mdns::is_local("printer.local")
        || !mdns::is_local("Printer.LOCAL.")
        || mdns::is_local("printer.example.com")
        || mdns::is_local("local.example")
    {
        return Err(193);
    }

    // --- The responder answers only its own `.local` name. ---
    let mut resp = mdns::Responder::new(MDNS_NAME, MDNS_ADDR);
    let (bytes, unicast) = resp.respond(MDNS_QUERY).ok_or(194)?;
    if bytes != MDNS_RESPONSE || unicast {
        return Err(195);
    }
    // The QU form is answered too, and the unicast preference is reported.
    let (_, unicast) = resp.respond(MDNS_QUERY_QU).ok_or(196)?;
    if !unicast || resp.answered() != 2 {
        return Err(197);
    }
    // Somebody else's name is not answered...
    let foreign = mdns::build_query("scanner.local", QType::A, false).ok_or(198)?;
    if resp.respond(&foreign).is_some() {
        return Err(199);
    }
    // ...and neither is a non-`.local` name, even though its first label matches
    // ours - a responder must never answer outside the `.local` namespace.
    let mut buf = [0u8; 128];
    let n = dns::build_query(0, "printer.example.com", QType::A, &mut buf).ok_or(200)?;
    if resp.respond(&buf[..n]).is_some() {
        return Err(203);
    }
    if resp.answered() != 2 {
        return Err(204);
    }
    // The unsolicited announcement is the same authoritative flush record.
    if resp.announcement().ok_or(205)? != MDNS_RESPONSE {
        return Err(206);
    }
    Ok(())
}

// --------------------------------------------------------------- 5. NTP

// The known-answer test. Four timestamps, chosen so the arithmetic is checkable by
// hand:
//
//   S  = 3_913_056_000 seconds since 1900 == 1_704_067_200 Unix == 2024-01-01T00:00Z
//   T1 = S + 0.0   (we sent)
//   T2 = S + 1.0   (server received)
//   T3 = S + 1.5   (server replied)
//   T4 = S + 2.0   (we received)
//
//   offset = ((T2-T1) + (T3-T4)) / 2 = ( (+1.0) + (-0.5) ) / 2 = +0.25 s
//   delay  = (T4-T1) - (T3-T2)       =   2.0    -   0.5        =  1.5  s
//
// so the server is 250 ms ahead of us and the round trip took 1.5 s.
const S_1900: u32 = 3_913_056_000;
const T1: Timestamp = Timestamp::from_parts(S_1900, 0);
const T2: Timestamp = Timestamp::from_parts(S_1900 + 1, 0);
/// `0x8000_0000` is exactly one half in 32.32 fixed point.
const T3: Timestamp = Timestamp::from_parts(S_1900 + 1, 0x8000_0000);
const T4: Timestamp = Timestamp::from_parts(S_1900 + 2, 0);

const EXPECT_OFFSET_NS: i64 = 250_000_000;
const EXPECT_DELAY_NS: u64 = 1_500_000_000;
/// `delay/2`, with a zero root delay/dispersion.
const EXPECT_ERROR_NS: u64 = 750_000_000;
/// `T4 + offset`, in nanoseconds since the Unix epoch:
/// `1_704_067_202 s + 250 ms`.
const EXPECT_CENTER_NS: i128 = 1_704_067_202_250_000_000;

/// The 48-byte client request for `T1`. `0x23` is LI 0 / VN 4 / Mode 3; everything
/// but the transmit timestamp is zero (RFC 4330 §4 permits an SNTP client to leave
/// stratum/poll/precision unset).
const NTP_REQUEST: &[u8] = &[
    0x23, 0x00, 0x00, 0x00, // LI/VN/Mode, stratum, poll, precision
    0x00, 0x00, 0x00, 0x00, // root delay
    0x00, 0x00, 0x00, 0x00, // root dispersion
    0x00, 0x00, 0x00, 0x00, // reference id
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reference timestamp
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // originate
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // receive
    0xe9, 0x3c, 0x7f, 0x00, 0x00, 0x00, 0x00, 0x00, // transmit = T1
];

/// The header fields of a crafted server reply, grouped so the encoder below takes
/// the header and the three timestamps rather than ten positional arguments.
#[derive(Copy, Clone)]
struct NtpHeader {
    li: u8,
    vn: u8,
    mode: u8,
    stratum: u8,
    root_delay: u32,
    root_dispersion: u32,
    ref_id: [u8; 4],
}

impl NtpHeader {
    /// A well-formed stratum-2 server header (`GPS` reference) - what every valid
    /// fixture starts from; a rejection test then changes exactly one field.
    const fn good() -> NtpHeader {
        NtpHeader {
            li: 0,
            vn: 4,
            mode: ntp::MODE_SERVER,
            stratum: 2,
            root_delay: 0,
            root_dispersion: 0,
            ref_id: *b"GPS\0",
        }
    }
}

/// Encode a server reply with the given header and timestamps (a small server-side
/// encoder, so the KAT bytes are produced and consumed by code rather than hand-typed
/// hex).
fn ntp_reply(
    h: NtpHeader,
    originate: Timestamp,
    receive: Timestamp,
    transmit: Timestamp,
) -> [u8; ntp::PACKET_LEN] {
    let mut p = [0u8; ntp::PACKET_LEN];
    p[0] = (h.li << 6) | (h.vn << 3) | h.mode;
    p[1] = h.stratum;
    p[4..8].copy_from_slice(&h.root_delay.to_be_bytes());
    p[8..12].copy_from_slice(&h.root_dispersion.to_be_bytes());
    p[12..16].copy_from_slice(&h.ref_id);
    p[24..32].copy_from_slice(&originate.0.to_be_bytes());
    p[32..40].copy_from_slice(&receive.0.to_be_bytes());
    p[40..48].copy_from_slice(&transmit.0.to_be_bytes());
    p
}

/// A good stratum-2 reply with the given root delay/dispersion.
fn ntp_good(root_delay: u32, root_dispersion: u32) -> [u8; ntp::PACKET_LEN] {
    let h = NtpHeader {
        root_delay,
        root_dispersion,
        ..NtpHeader::good()
    };
    ntp_reply(h, T1, T2, T3)
}

fn ntp_ok() -> Result<Estimate, i32> {
    // --- The request byte oracle. ---
    if T1.secs() != S_1900 || T1.frac() != 0 {
        return Err(210);
    }
    let req = ntp::build_client_request(T1);
    if req != NTP_REQUEST {
        return Err(211);
    }
    // And the timestamp helpers agree with the epoch arithmetic.
    if T1.as_unix_ns() != 1_704_067_200_000_000_000 {
        return Err(212);
    }
    if Timestamp::from_unix(1_704_067_200, 500_000_000)
        != Timestamp::from_parts(S_1900, 0x8000_0000)
    {
        return Err(213);
    }

    // --- The KAT: offset and delay exactly as hand-computed. ---
    let pkt = ntp::parse(&ntp_good(0, 0)).map_err(|_| 214)?;
    if pkt.li != 0 || pkt.vn != 4 || pkt.mode != ntp::MODE_SERVER || pkt.stratum != 2 {
        return Err(215);
    }
    if pkt.originate != T1 || pkt.receive != T2 || pkt.transmit != T3 {
        return Err(216);
    }
    pkt.validate(T1).map_err(|_| 217)?;
    let sample = ntp::Sample::compute(&pkt, T1, T4).map_err(|_| 218)?;
    if sample.offset_ns != EXPECT_OFFSET_NS {
        return Err(219);
    }
    if sample.delay_ns != EXPECT_DELAY_NS {
        return Err(220);
    }
    if sample.root_distance_ns != 0 || sample.stratum != 2 {
        return Err(221);
    }

    // --- The result is a bounded interval, not an instant. ---
    let est = sample.estimate(T4);
    if est.center_unix_ns != EXPECT_CENTER_NS {
        return Err(222);
    }
    if est.error_ns != EXPECT_ERROR_NS {
        return Err(223);
    }
    if est.lower_unix_ns() != EXPECT_CENTER_NS - EXPECT_ERROR_NS as i128
        || est.upper_unix_ns() != EXPECT_CENTER_NS + EXPECT_ERROR_NS as i128
    {
        return Err(224);
    }
    if est.width_ns() != 2 * EXPECT_ERROR_NS {
        return Err(225);
    }
    // The truth (T3, the server's own transmit time) must lie inside the interval,
    // and a value a second outside must not.
    if !est.contains(T3.as_unix_ns()) {
        return Err(226);
    }
    if est.contains(EXPECT_CENTER_NS + 2_000_000_000) {
        return Err(227);
    }

    // --- The server's declared distance widens the interval, exactly. ---
    // root delay 1.0 s (0x0001_0000 in 16.16) + root dispersion 0.5 s (0x0000_8000)
    // -> root_distance = 0.5 + 1.0/2 = 1.0 s, so error = 750 ms + 1 s = 1.75 s.
    let pkt2 = ntp::parse(&ntp_good(0x0001_0000, 0x0000_8000)).map_err(|_| 228)?;
    if pkt2.root_delay_ns() != 1_000_000_000 || pkt2.root_dispersion_ns() != 500_000_000 {
        return Err(229);
    }
    let s2 = ntp::Sample::compute(&pkt2, T1, T4).map_err(|_| 230)?;
    if s2.root_distance_ns != 1_000_000_000 {
        return Err(231);
    }
    let e2 = s2.estimate(T4);
    if e2.error_ns != 1_750_000_000 || e2.center_unix_ns != EXPECT_CENTER_NS {
        return Err(232);
    }

    // --- Rejections, each with its own reason. ---
    if ntp::parse(&NTP_REQUEST[..47]) != Err(NtpError::Short) {
        return Err(240);
    }
    let bad_mode = ntp_reply(
        NtpHeader {
            mode: ntp::MODE_CLIENT,
            ..NtpHeader::good()
        },
        T1,
        T2,
        T3,
    );
    if ntp::parse(&bad_mode).map_err(|_| 241)?.validate(T1) != Err(NtpError::BadMode) {
        return Err(242);
    }
    let bad_ver = ntp_reply(
        NtpHeader {
            vn: 2,
            ..NtpHeader::good()
        },
        T1,
        T2,
        T3,
    );
    if ntp::parse(&bad_ver).map_err(|_| 243)?.validate(T1) != Err(NtpError::BadVersion) {
        return Err(244);
    }
    let kod = ntp_reply(
        NtpHeader {
            stratum: 0,
            ref_id: *b"DENY",
            ..NtpHeader::good()
        },
        T1,
        T2,
        T3,
    );
    let kp = ntp::parse(&kod).map_err(|_| 245)?;
    if kp.validate(T1) != Err(NtpError::KissOfDeath) || kp.kod() != Some(ntp::KodCode::DENY) {
        return Err(246);
    }
    let bad_stratum = ntp_reply(
        NtpHeader {
            stratum: 16,
            ..NtpHeader::good()
        },
        T1,
        T2,
        T3,
    );
    if ntp::parse(&bad_stratum).map_err(|_| 247)?.validate(T1) != Err(NtpError::BadStratum) {
        return Err(248);
    }
    let unsync = ntp_reply(
        NtpHeader {
            li: ntp::LI_UNSYNC,
            ..NtpHeader::good()
        },
        T1,
        T2,
        T3,
    );
    if ntp::parse(&unsync).map_err(|_| 249)?.validate(T1) != Err(NtpError::Unsynchronized) {
        return Err(250);
    }
    let zero_tx = ntp_reply(NtpHeader::good(), T1, T2, Timestamp::ZERO);
    if ntp::parse(&zero_tx).map_err(|_| 251)?.validate(T1) != Err(NtpError::ZeroTransmit) {
        return Err(252);
    }
    // An answer to a request we never sent (the off-path injection defence).
    let wrong_origin = ntp_reply(
        NtpHeader::good(),
        Timestamp::from_parts(S_1900 - 100, 0),
        T2,
        T3,
    );
    if ntp::parse(&wrong_origin).map_err(|_| 253)?.validate(T1) != Err(NtpError::OriginateMismatch)
    {
        return Err(254);
    }
    // Timestamps that run backwards are nonsense, not a negative delay.
    let backwards = ntp_reply(NtpHeader::good(), T1, T3, T2);
    let bp = ntp::parse(&backwards).map_err(|_| 255)?;
    if ntp::Sample::compute(&bp, T1, T4) != Err(NtpError::BadTimestamps) {
        return Err(256);
    }

    // --- The client: the request/reply pairing and the poll schedule. ---
    let mut c = ntp::Client::new();
    if c.poll_interval_ns() != 64 * NS {
        return Err(260);
    }
    let _ = c.build_request(T1);
    let s = c.on_reply(&ntp_good(0, 0), T4).map_err(|_| 261)?;
    if s.offset_ns != EXPECT_OFFSET_NS {
        return Err(262);
    }
    // The correction is a **userspace** offset with its bound - no clock was touched.
    if c.correction() != (EXPECT_OFFSET_NS, EXPECT_ERROR_NS) {
        return Err(263);
    }
    if c.counts() != (1, 0) {
        return Err(264);
    }
    // A timeout backs the poll interval off; a Kiss-o'-Death does too (ignoring one
    // is how a client gets blocked).
    c.on_timeout();
    if c.poll_interval_ns() != 128 * NS {
        return Err(265);
    }
    let _ = c.build_request(T1);
    if c.on_reply(&kod, T4) != Err(NtpError::KissOfDeath) {
        return Err(266);
    }
    if c.poll_interval_ns() != 256 * NS {
        return Err(267);
    }
    // Backoff is capped at MAXPOLL (2^10 s).
    for _ in 0..10 {
        c.on_timeout();
    }
    if c.poll_interval_ns() != 1024 * NS {
        return Err(268);
    }
    Ok(est)
}

// --------------------------------------------------------- live (bonus)

// Every live phase is bounded by a **duration**, never by a drain count. That is
// the honest unit (a poll count buys a different amount of listening on each ISA -
// docs/NETSTACK.md 16) and it is what keeps the whole cell inside the test budget:
// the four phases below can cost at most a few seconds of wall clock between them,
// and where the kernel can idle they cost almost no CPU.

/// How long the live link-local claim listens after each ARP probe: **200 ms**.
/// RFC 3927 §2.2.1 waits one to two seconds; this is a *bonus* liveness check (the
/// conflict protocol is proven deterministically above), so it only needs long enough
/// for an on-link answer to come back. Paid at most `PROBE_COUNT * (1 + PROBE_COUNT)`
/// times, so the phase is bounded by ~2.4 s even if every round conflicts.
const PROBE_WINDOW_NS: u64 = 200_000_000;

/// How long the live mDNS query waits for a responder: **500 ms**. There is no mDNS
/// peer on the emulated link, so this is the price of proving the multicast frame went
/// out and nothing answered.
const MDNS_WINDOW_NS: u64 = 500_000_000;

/// Attempt a real DHCP DISCOVER on the wire.
///
/// QEMU's SLIRP **does** run a DHCP server on the emulated link (that is how a normal
/// guest gets `10.0.2.15`), so unlike the NTP and mDNS phases this one is normally
/// answered - and the lease that comes back is entirely real: our DISCOVER, SLIRP's
/// OFFER, our REQUEST, SLIRP's ACK, decoded by the same parser the deterministic
/// oracles exercise. It is still reported rather than asserted: a lease is a property
/// of the QEMU network backend, not of this code, and nothing here **ever**
/// synthesises one - a link with no server prints the skip instead.
async fn live_dhcp(mac: Mac) {
    let mut client = dhcp::Client::new(mac.0, librheo::rng::next_u64());
    let mut cfg = HostConfig::new();
    match dhcp::configure(&mut client, mac, &mut cfg).await {
        Ok(lease) => {
            println!(
                "nethostcfg-demo: LIVE DHCP lease {}.{}.{}.{}/{} gw {}.{}.{}.{} for {}s from a real \
                 DISCOVER->OFFER->REQUEST->ACK with SLIRP's server; the hostcfg store is now {:?}",
                lease.address.0[0],
                lease.address.0[1],
                lease.address.0[2],
                lease.address.0[3],
                cfg.prefix_len().unwrap_or(0),
                lease.router.unwrap_or(Ipv4Addr::new(0, 0, 0, 0)).0[0],
                lease.router.unwrap_or(Ipv4Addr::new(0, 0, 0, 0)).0[1],
                lease.router.unwrap_or(Ipv4Addr::new(0, 0, 0, 0)).0[2],
                lease.router.unwrap_or(Ipv4Addr::new(0, 0, 0, 0)).0[3],
                lease.lease_secs,
                cfg.source()
            );
        }
        Err(e) => {
            println!(
                "nethostcfg-demo: live DHCP skipped - {} DISCOVER(s) sent, no server answered on \
                 the emulated link within {} ms each ({:?})",
                client.sent(),
                dhcp::RECV_WINDOW_NS / 1_000_000,
                e
            );
        }
    }
}

/// Attempt a real NTP request to the SLIRP gateway. SLIRP runs no NTP service, so a
/// timeout is expected. Even a reply could not be turned into a synced clock: a cell
/// has no nanosecond wall clock to fill T1/T4 with (see `net::ntp`). Nothing about
/// time is ever asserted from this.
async fn live_ntp(mac: Mac) {
    let cfg = HostConfig::slirp();
    let mut udp = UdpEndpoint::from_host_config(mac, &cfg);
    let server = slirp_gateway();
    let mut reply = [0u8; 128];
    // T1 is a placeholder: we have no wall clock, which is exactly the limitation.
    match ntp::query(&mut udp, server, T1, &mut reply, ntp::REPLY_TIMEOUT_NS).await {
        Ok(n) => match ntp::parse(&reply[..n]) {
            Ok(p) => println!(
                "nethostcfg-demo: LIVE NTP reply from {}.{}.{}.{} stratum {} - structure only; \
                 no offset computed (a cell has no ns wall clock for T1/T4)",
                server.0[0], server.0[1], server.0[2], server.0[3], p.stratum
            ),
            Err(e) => println!("nethostcfg-demo: live NTP reply unparseable ({e:?}) - tolerated"),
        },
        Err(e) => println!(
            "nethostcfg-demo: live NTP skipped - no answer from {}.{}.{}.{}:123 within {} ms \
             ({:?}); SLIRP runs no NTP service",
            server.0[0],
            server.0[1],
            server.0[2],
            server.0[3],
            ntp::REPLY_TIMEOUT_NS / 1_000_000,
            e
        ),
    }
}

/// Attempt a real mDNS query to `224.0.0.251:5353`. The frame genuinely goes out
/// (multicast needs no IGMP for this link-local group - see `net::zeroconf`), but
/// there is no mDNS peer on the emulated link, so no answer is expected.
async fn live_mdns(mac: Mac) {
    let cfg = HostConfig::slirp();
    match mdns::query(mac, cfg.source_address(), "printer.local", MDNS_WINDOW_NS).await {
        Ok(records) if records.is_empty() => println!(
            "nethostcfg-demo: live mDNS skipped - query multicast to 224.0.0.251:5353 (MAC \
             01:00:5e:00:00:fb), no responder answered within {} ms on the emulated link",
            MDNS_WINDOW_NS / 1_000_000
        ),
        Ok(records) => println!(
            "nethostcfg-demo: LIVE mDNS answered with {} record(s) (unexpected here, but real)",
            records.len()
        ),
        Err(e) => println!("nethostcfg-demo: live mDNS transmit failed ({e:?}) - tolerated"),
    }
}

/// Attempt a real link-local claim on the wire: the ARP probes genuinely go out.
/// Absence of a conflict is absence of evidence, not proof the address is free -
/// conflict *detection* is what the deterministic checks prove.
async fn live_linklocal(mac: Mac) {
    let mut ll = LinkLocal::new(mac, librheo::rng::next_u64());
    match zeroconf::claim(&mut ll, PROBE_WINDOW_NS, 0).await {
        Ok(addr) => println!(
            "nethostcfg-demo: live link-local probed+announced {}.{}.{}.{} - {} probe(s) sent, {} \
             ms listened after each, no conflicting ARP seen (absence of evidence: this proves the \
             frames went out, not that the address is free)",
            addr.0[0],
            addr.0[1],
            addr.0[2],
            addr.0[3],
            zeroconf::PROBE_COUNT,
            PROBE_WINDOW_NS / 1_000_000
        ),
        Err(e) => println!("nethostcfg-demo: live link-local claim did not complete ({e:?})"),
    }
}

// --------------------------------------------------------------- main

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("nethostcfg-demo: all host-config assertions OK");
    OK_CODE
}

async fn run() {
    // --- The deterministic core (network-free). ---
    let lease = match dhcp_ok() {
        Ok(l) => l,
        Err(c) => return fail(c),
    };
    println!(
        "nethostcfg-demo: DHCP codec + DISCOVER byte oracle + \
         DISCOVER->OFFER->REQUEST->ACK->BOUND + T1/T2 renewal + rebind + expiry + NAK + \
         7 rejections + DECLINE/RELEASE OK"
    );

    let mac = match net::mac().await {
        Ok(m) => m,
        // No NIC: the deterministic checks still stand, but the endpoint read-back
        // and the live attempts need a MAC. Use a fixed one rather than skipping.
        Err(_) => Mac(MAC),
    };

    if let Err(c) = hostcfg_ok(&lease, mac) {
        return fail(c);
    }
    println!(
        "nethostcfg-demo: hostcfg store (lease -> address/mask/gateway/DNS/search, on-link vs \
         gateway routing, link-local clears the gateway) + read back by dns::Config and \
         udp::UdpEndpoint OK"
    );

    if let Err(c) = linklocal_ok() {
        return fail(c);
    }
    println!(
        "nethostcfg-demo: IPv4 link-local claim (candidate KAT, 0.0.0.0 ARP probe, conflict \
         re-pick, racing probe, 3 probes + 2 announces -> claimed and announcing then bounded, \
         defend-once-then-yield) OK"
    );

    if let Err(c) = mdns_ok() {
        return fail(c);
    }
    println!(
        "nethostcfg-demo: mDNS over the DNS codec (query + QU oracle, response + cache-flush + \
         goodbye, 01:00:5e multicast MAC, .local scoping, responder) OK"
    );

    let est = match ntp_ok() {
        Ok(e) => e,
        Err(c) => return fail(c),
    };
    println!(
        "nethostcfg-demo: NTP KAT - offset +{} ns, delay {} ns, interval half-width {} ns \
         (a bounded interval, NOT a disciplined clock) + 9 rejections + KoD backoff OK",
        EXPECT_OFFSET_NS, EXPECT_DELAY_NS, est.error_ns
    );

    // --- Bonus live attempts. None is fatal; none may fake a result. ---
    live_dhcp(mac).await;
    live_ntp(mac).await;
    live_mdns(mac).await;
    live_linklocal(mac).await;

    // Success: CODE stays 0.
}
