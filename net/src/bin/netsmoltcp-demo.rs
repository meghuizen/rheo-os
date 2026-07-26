//! `netsmoltcp-demo` - the rheo-net Phase N2c proof cell (docs/NETSTACK.md §13).
//! Two deliverables in one cell, both honest about the single-CPU model:
//!
//! **(A) The smoltcp blessed transport cell.** smoltcp - the doc-named blessed
//! pure-Rust `no_std` transport - drives our real virtio-net driver over
//! `librheo::net`, and also runs a deterministic in-cell exchange over smoltcp's
//! own `Loopback` device:
//!   1. **Deterministic (network-free):** a smoltcp TCP client + server over the
//!      built-in `Loopback` device complete a handshake and transfer bytes, and a
//!      smoltcp UDP socket pair round-trips a datagram over `Loopback`. This
//!      proves the smoltcp integration (Device trait, Interface, SocketSet, poll
//!      loop, alloc, the ms clock) with no network.
//!   2. **Live (over SLIRP):** a smoltcp UDP socket sends a DNS query to SLIRP's
//!      built-in responder `10.0.2.3:53` over a `QueueDevice` bound to
//!      `librheo::net`, and receives the reply - proving smoltcp drives the NIC
//!      end to end. Asserted (SLIRP answers deterministically, as in `netl4`).
//!
//! **(B) The native sharded transport framing.** A `net::shard::Transport` of two
//! shards: connections hash to shards by their 4-tuple, `connect`/`listen` route
//! to the owning shard, and a cross-shard connection pair drives a real TCP
//! handshake + byte exchange over the in-cell `VirtualLink`. Shared-nothing:
//! each shard owns a disjoint connection set. **Honest:** on the single CPU the
//! shards interleave - structural isolation, not parallel throughput (SMP #27).
//!
//! It exits `0x42` only if every deterministic step passes (the live SLIRP UDP is
//! asserted too, matching `netl4`'s confidence). The kernel is untouched -
//! portable userspace over the existing `OP_NET_*` queue path; smoltcp is the one
//! doc-named dependency (pinned `=0.13.1`, `no_std`, behind the `smoltcp` feature).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};

use rheo_net::ip::Ipv4Addr;
use rheo_net::shard::{FourTuple, Transport};
use rheo_net::smoltcp_cell::{Clock, QueueDevice, pump};
use rheo_net::tcp::{Connection, FixedWindow, VirtualLink};
use rheo_net::timer::TimerWheel;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, Loopback, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's guest address, gateway, and built-in DNS responder.
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const DNS_IP: [u8; 4] = [10, 0, 2, 3];
const DNS_PORT: u16 = 53;

/// A minimal DNS query (`A example.com`, transaction id `0x1234`) - the same
/// 29-byte datagram `netl4` sends; we only check the id echoes back.
const DNS_TXID: u16 = 0x1234;
const DNS_QUERY: [u8; 29] = [
    0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x', b'a',
    b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
];

/// Poll budget for the live SLIRP round trip (each poll sleeps POLL_MS).
const LIVE_POLLS: u32 = 400;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("netsmoltcp-demo: smoltcp cell + native sharded transport OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

async fn run() {
    // --- (B) Native sharded transport (deterministic, network-free). ---
    if let Err(e) = sharded_transport() {
        return fail(e);
    }

    // --- (A1) smoltcp in-cell over Loopback (deterministic, network-free). ---
    if let Err(e) = smoltcp_loopback_tcp() {
        return fail(e);
    }
    if let Err(e) = smoltcp_loopback_udp() {
        return fail(e);
    }

    // --- (A2) smoltcp live UDP over SLIRP's NIC path. ---
    if let Err(e) = smoltcp_live_udp().await {
        fail(e);
    }
}

// ===================================================================
// (B) Native sharded transport
// ===================================================================

/// Two shards; connections hash to shards; the connection set **partitions**
/// across both shards (disjoint, deterministic); and a shard-owned connection in
/// **each** shard completes a real TCP handshake + byte transfer over the in-cell
/// virtual link. Shared-nothing: each shard owns a disjoint connection set.
fn sharded_transport() -> Result<(), i32> {
    let mut xport = Transport::new(2);
    if xport.shard_count() != 2 {
        return Err(70);
    }

    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let mk = |port: u16| FourTuple {
        local_ip: cip,
        local_port: port,
        remote_ip: sip,
        remote_port: 80,
    };

    // Determinism: the same tuple always maps to the same shard.
    if xport.shard_index(&mk(40000)) != xport.shard_index(&mk(40000)) {
        return Err(71);
    }

    // Route a set of connections; record one that landed in each shard so we can
    // later prove "each shard's connections work".
    let mut in_shard: [Option<FourTuple>; 2] = [None, None];
    let mut counts = [0usize; 2];
    for port in 40000u16..40032 {
        let t = mk(port);
        let idx = xport.connect(t, 0x0000_1000);
        // The connection must live in exactly the shard its hash names, and in
        // no other shard (shared-nothing / disjoint ownership).
        if !xport.shard(idx).contains(&t) {
            return Err(72);
        }
        if xport.shard(idx ^ 1).contains(&t) {
            return Err(73);
        }
        counts[idx] += 1;
        if in_shard[idx].is_none() {
            in_shard[idx] = Some(t);
        }
    }
    // The 4-tuple hash must partition the set across BOTH shards (not degenerate).
    if counts[0] == 0 || counts[1] == 0 {
        return Err(74);
    }
    if xport.shard(0).len() != counts[0] || xport.shard(1).len() != counts[1] {
        return Err(75);
    }

    // Each shard's connections work: drive a shard-0-owned and a shard-1-owned
    // connection each through a full handshake + transfer against a locally-held
    // peer (the remote host - it is NOT in this transport; the transport owns only
    // the local end, the shared-nothing boundary).
    for (shard, slot) in in_shard.iter().enumerate() {
        let ct = slot.ok_or(76)?;
        if xport.shard_index(&ct) != shard {
            return Err(77);
        }
        drive_shard_conn(&mut xport, &ct)?;
    }

    println!(
        "netsmoltcp-demo: sharded transport - 2 shards ({}+{} conns), each shard's TCP works (structural isolation, single-CPU)",
        counts[0], counts[1]
    );
    Ok(())
}

/// Drive the shard-owned connection `ct` through a full TCP handshake + byte
/// transfer against a locally-held peer (the remote host, mirror-addressed). The
/// connection is fetched from its shard each step; the peer is independent of the
/// transport (no shared state).
fn drive_shard_conn(xport: &mut Transport, ct: &FourTuple) -> Result<(), i32> {
    let st = ct.mirror();
    let mut server: Conn = Connection::listen(
        st.local_ip,
        st.local_port,
        st.remote_ip,
        st.remote_port,
        0x0000_9000,
    );
    let mut link = VirtualLink::new();
    let mut wheel = TimerWheel::new();

    // Handshake to ESTABLISHED.
    for _ in 0..128 {
        let c = xport.get_mut(ct).ok_or(78)?;
        if !pump_pair(c, &mut server, &mut link, &mut wheel) {
            break;
        }
    }
    const MSG: &[u8] = b"sharded transport: per-shard TCP over rheo-net";
    {
        let c = xport.get_mut(ct).ok_or(78)?;
        if !c.is_established() || !server.is_established() {
            return Err(79);
        }
        if c.write(MSG) != MSG.len() {
            return Err(80);
        }
        drain_pair(c, &mut server, &mut link, &mut wheel);
    }
    let mut buf = [0u8; 64];
    let n = server.read(&mut buf);
    if n != MSG.len() || &buf[..n] != MSG {
        return Err(81);
    }
    Ok(())
}

type Conn = Connection<FixedWindow>;

/// One handshake step: poll both ends, transfer their segments. Returns whether
/// anything moved (so the caller can advance the clock when quiescent).
fn pump_pair(a: &mut Conn, b: &mut Conn, link: &mut VirtualLink, wheel: &mut TimerWheel) -> bool {
    let now = wheel.now();
    let mut moved = false;
    while let Some(s) = a.poll(now) {
        link.transfer(&s, b, now);
        moved = true;
    }
    while let Some(s) = b.poll(now) {
        link.transfer(&s, a, now);
        moved = true;
    }
    if !moved {
        wheel.set_now(now + 1);
    }
    moved
}

/// Drive both ends to quiescence (deliver all pending data/acks).
fn drain_pair(a: &mut Conn, b: &mut Conn, link: &mut VirtualLink, wheel: &mut TimerWheel) {
    for _ in 0..1024 {
        if !pump_pair(a, b, link, wheel) {
            break;
        }
    }
}

// ===================================================================
// (A) smoltcp
// ===================================================================

/// A smoltcp Ethernet interface with the SLIRP guest IP over `device`.
fn make_iface<D: Device + ?Sized>(device: &mut D, mac: [u8; 6], ip: [u8; 4]) -> Interface {
    let config = Config::new(EthernetAddress(mac).into());
    let mut iface = Interface::new(config, device, Instant::from_millis(0));
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), 24));
    });
    iface
}

/// smoltcp TCP over the built-in Loopback device: a client connects to a server,
/// server sends bytes, client receives them - all in-cell, no network.
fn smoltcp_loopback_tcp() -> Result<(), i32> {
    let mut device = Loopback::new(Medium::Ethernet);
    let mut iface = make_iface(&mut device, [0x02, 0, 0, 0, 0, 1], [127, 0, 0, 1]);

    let mut srv_rx = [0u8; 512];
    let mut srv_tx = [0u8; 512];
    let mut cli_rx = [0u8; 512];
    let mut cli_tx = [0u8; 512];
    let server = tcp::Socket::new(
        tcp::SocketBuffer::new(&mut srv_rx[..]),
        tcp::SocketBuffer::new(&mut srv_tx[..]),
    );
    let client = tcp::Socket::new(
        tcp::SocketBuffer::new(&mut cli_rx[..]),
        tcp::SocketBuffer::new(&mut cli_tx[..]),
    );
    let mut sockets: [smoltcp::iface::SocketStorage; 2] = Default::default();
    let mut sockets = SocketSet::new(&mut sockets[..]);
    let sh = sockets.add(server);
    let ch = sockets.add(client);

    let payload = b"smoltcp loopback tcp";
    let mut did_listen = false;
    let mut did_connect = false;
    let mut got = 0usize;
    let mut buf = [0u8; 64];

    for clock_ms in 0..200i64 {
        iface.poll(Instant::from_millis(clock_ms), &mut device, &mut sockets);

        let s = sockets.get_mut::<tcp::Socket>(sh);
        if !s.is_open() && !did_listen {
            s.listen(IpListenEndpoint::from(1234)).map_err(|_| 90)?;
            did_listen = true;
        }
        if s.can_send() && did_connect {
            let _ = s.send_slice(payload);
            s.close();
        }

        let c = sockets.get_mut::<tcp::Socket>(ch);
        if !did_connect {
            let cx = iface.context();
            c.connect(
                cx,
                IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 1234),
                IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 49500),
            )
            .map_err(|_| 91)?;
            did_connect = true;
        }
        if c.can_recv() {
            got = c.recv_slice(&mut buf).map_err(|_| 92)?;
            if got > 0 {
                break;
            }
        }
    }

    if got != payload.len() || &buf[..got] != payload {
        return Err(93);
    }
    println!("netsmoltcp-demo: smoltcp Loopback TCP handshake + transfer OK");
    Ok(())
}

/// smoltcp UDP over the built-in Loopback device: one socket sends a datagram to
/// another on the same interface; the receiver reads it back - in-cell.
fn smoltcp_loopback_udp() -> Result<(), i32> {
    let mut device = Loopback::new(Medium::Ethernet);
    let mut iface = make_iface(&mut device, [0x02, 0, 0, 0, 0, 2], [127, 0, 0, 1]);

    let mut a_rxm = [udp::PacketMetadata::EMPTY; 4];
    let mut a_rxp = [0u8; 512];
    let mut a_txm = [udp::PacketMetadata::EMPTY; 4];
    let mut a_txp = [0u8; 512];
    let mut b_rxm = [udp::PacketMetadata::EMPTY; 4];
    let mut b_rxp = [0u8; 512];
    let mut b_txm = [udp::PacketMetadata::EMPTY; 4];
    let mut b_txp = [0u8; 512];
    let sock_a = udp::Socket::new(
        udp::PacketBuffer::new(&mut a_rxm[..], &mut a_rxp[..]),
        udp::PacketBuffer::new(&mut a_txm[..], &mut a_txp[..]),
    );
    let sock_b = udp::Socket::new(
        udp::PacketBuffer::new(&mut b_rxm[..], &mut b_rxp[..]),
        udp::PacketBuffer::new(&mut b_txm[..], &mut b_txp[..]),
    );
    let mut sockets: [smoltcp::iface::SocketStorage; 2] = Default::default();
    let mut sockets = SocketSet::new(&mut sockets[..]);
    let ha = sockets.add(sock_a);
    let hb = sockets.add(sock_b);

    sockets
        .get_mut::<udp::Socket>(ha)
        .bind(IpListenEndpoint::from(6000))
        .map_err(|_| 95)?;
    sockets
        .get_mut::<udp::Socket>(hb)
        .bind(IpListenEndpoint::from(6001))
        .map_err(|_| 96)?;

    let payload = b"smoltcp loopback udp";
    sockets
        .get_mut::<udp::Socket>(ha)
        .send_slice(payload, IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 6001))
        .map_err(|_| 97)?;

    let mut buf = [0u8; 64];
    let mut got = 0usize;
    for clock_ms in 0..200i64 {
        iface.poll(Instant::from_millis(clock_ms), &mut device, &mut sockets);
        let b = sockets.get_mut::<udp::Socket>(hb);
        if b.can_recv() {
            let (n, _meta) = b.recv_slice(&mut buf).map_err(|_| 98)?;
            got = n;
            break;
        }
    }
    if got != payload.len() || &buf[..got] != payload {
        return Err(99);
    }
    println!("netsmoltcp-demo: smoltcp Loopback UDP round trip OK");
    Ok(())
}

/// smoltcp UDP over the real NIC (QueueDevice + librheo::net): a DNS query to
/// SLIRP's `10.0.2.3:53`, reply asserted (transaction id echoed). This is the
/// end-to-end proof that smoltcp drives our virtio-net driver.
async fn smoltcp_live_udp() -> Result<(), i32> {
    let mac = match net::mac().await {
        Ok(m) => m.0,
        Err(_) => return Err(60),
    };

    let mut device = QueueDevice::new();
    let mut clock = Clock::new();
    let mut iface = make_iface(&mut device, mac, GUEST_IP);

    let mut rxm = [udp::PacketMetadata::EMPTY; 8];
    let mut rxp = vec![0u8; 4096];
    let mut txm = [udp::PacketMetadata::EMPTY; 8];
    let mut txp = vec![0u8; 4096];
    let sock = udp::Socket::new(
        udp::PacketBuffer::new(&mut rxm[..], &mut rxp[..]),
        udp::PacketBuffer::new(&mut txm[..], &mut txp[..]),
    );
    let mut sockets: [smoltcp::iface::SocketStorage; 1] = Default::default();
    let mut sockets = SocketSet::new(&mut sockets[..]);
    let h = sockets.add(sock);
    sockets
        .get_mut::<udp::Socket>(h)
        .bind(IpListenEndpoint::from(0x9876))
        .map_err(|_| 61)?;

    let dst = IpEndpoint::new(
        IpAddress::v4(DNS_IP[0], DNS_IP[1], DNS_IP[2], DNS_IP[3]),
        DNS_PORT,
    );
    // Enqueue the DNS query (smoltcp resolves the next hop via ARP during poll).
    sockets
        .get_mut::<udp::Socket>(h)
        .send_slice(&DNS_QUERY, dst)
        .map_err(|_| 66)?;

    let mut buf = [0u8; 2048];
    let mut got = None;
    let mut rx_total = 0usize;
    for i in 0..LIVE_POLLS {
        // Canonical order: fill RX off the NIC, poll (process RX + generate TX),
        // flush TX out the NIC, advancing the smoltcp clock by a real sleep.
        rx_total += pump(&mut device, &mut clock).await;
        iface.poll(clock.now(), &mut device, &mut sockets);

        let s = sockets.get_mut::<udp::Socket>(h);
        if s.can_recv()
            && let Ok((n, meta)) = s.recv_slice(&mut buf)
        {
            got = Some((n, meta));
            break;
        }
        // Periodic retransmit (a lost datagram is retried, not a hang).
        if i % 64 == 63 && s.can_send() {
            let _ = s.send_slice(&DNS_QUERY, dst);
        }
    }

    let (n, meta) = match got {
        Some(g) => g,
        None => {
            println!(
                "netsmoltcp-demo: smoltcp live UDP - no reply after {} polls ({} rx frames)",
                LIVE_POLLS, rx_total
            );
            return Err(62); // SLIRP DNS did not answer smoltcp
        }
    };
    // The reply must come from 10.0.2.3:53 and echo our transaction id.
    let from_ip = match meta.endpoint.addr {
        IpAddress::Ipv4(a) => a.octets(),
        _ => return Err(63),
    };
    if from_ip != DNS_IP || meta.endpoint.port != DNS_PORT {
        println!(
            "netsmoltcp-demo: smoltcp reply from {}.{}.{}.{}:{} unexpected",
            from_ip[0], from_ip[1], from_ip[2], from_ip[3], meta.endpoint.port
        );
        return Err(64);
    }
    if n < 2 || u16::from_be_bytes([buf[0], buf[1]]) != DNS_TXID {
        return Err(65);
    }
    println!(
        "netsmoltcp-demo: smoltcp LIVE UDP - DNS reply from 10.0.2.3:53 over virtio-net, {} B, txid {:#06x} echoed",
        n, DNS_TXID
    );
    Ok(())
}
