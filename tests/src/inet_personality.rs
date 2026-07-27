//! The **remote-INET datapath**: the `svc::SocketOps` bridge that makes real
//! remote networking work for unmodified Linux binaries (rheo-net **N4b**,
//! docs/NETSTACK.md N4b, docs/LINUX-COMPAT.md L8-INET remote).
//!
//! ## Why it lives here and not in the kernel
//! Doctrine (docs/ARCHITECTURE.md 6, docs/NETWORKING.md) puts IP/UDP/TCP in
//! **userspace**, and `kernel/` is **allocation-free**, so the kernel can hold no
//! network stack. It holds a *bridge* instead: `svc::SocketOps`, a table of
//! function pointers a service registers - exactly the `svc::FileOps` pattern that
//! keeps the kernel **filesystem-free** while still serving the POSIX file
//! syscalls, and exactly what `vfs_personality.rs` (this file's sibling) registers
//! for files. The Linux personality forwards every **non-loopback** socket
//! operation here; loopback keeps its in-kernel L6-ring fast path byte-for-byte
//! unchanged.
//!
//! ## What drives the wire
//! The protocol work is `rheo-net`, linked in its **codec posture**
//! (`--no-default-features`: no librheo, so no cell `_start`/panic handler/global
//! allocator to collide with a kernel's - docs/NETSTACK.md N4b). Everything on the
//! wire is the stack's own code: `eth` framing, `arp` request/reply, `ip` headers +
//! the Internet checksum, `udp` build/parse + the pseudo-header checksum, and the
//! full RFC 793 `tcp::Connection` state machine (its `poll(now) -> Option<Vec<u8>>`
//! / `on_wire_segment(now, bytes)` seam is synchronous and transport-independent,
//! so it drives cleanly from kernel context). Nothing is re-implemented here; this
//! module is the *driver loop* - resolve the next hop, hand frames to
//! `hw::virtio_net`, park on `net_rx::wait_frame_slice` for replies, demultiplex.
//!
//! ## Blocking
//! A receive **parks**: `net_rx::wait_frame_slice` is the N2d park-until-frame
//! primitive, so on riscv64/aarch64 the kernel genuinely halts at WFI until the
//! NIC's RX interrupt fires, and on x86-64 it falls back to the same documented
//! bounded poll (no MSI-X through the PCI config tunnel). Never a userspace
//! re-submit storm.
//!
//! ## Honest scope
//! IPv4 only (a v6 remote destination still gets `ENETUNREACH`); one datapath
//! instance for the whole machine (a per-cell instance is the service-cell step);
//! fixed-size registries; remote handles are not reference-counted across
//! `dup`/`fork`; and no TCP listener (a remote *inbound* connection needs steering
//! grants - docs/NETWORKING.md).

use core::ptr::addr_of_mut;

use kernel::arch;
use kernel::hw::virtio_net;
use kernel::net_rx;
use kernel::svc::SocketOps;

use rheo_net::eth::Mac;
use rheo_net::ip::Ipv4Addr;
use rheo_net::tcp::{Connection, FixedWindow, State};
use rheo_net::{arp, eth, ip, tcp, udp, wire};

// -------------------------------------------------------------- configuration

/// The datapath's own IPv4 address. QEMU SLIRP (`-netdev user`) always hands the
/// guest `10.0.2.15`, with the gateway at `10.0.2.2` and a built-in DNS responder
/// at `10.0.2.3` - so this is a fixed, deterministic identity, no DHCP needed
/// (DHCP as a userspace service is a documented later phase).
pub const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// The default gateway: the next hop for anything off the local /24.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Concurrent remote UDP endpoints.
const UDP_EPS: usize = 4;
/// Datagrams buffered per endpoint.
const UDP_QUEUE: usize = 4;
/// Largest UDP payload buffered (a standard-MTU datagram).
const UDP_MAX: usize = 1500;
/// Concurrent remote TCP connections.
const TCP_CONNS: usize = 4;
/// ARP cache entries.
const ARP_ENTRIES: usize = 4;

/// One receive attempt's park budget when draining towards a caller deadline.
/// Short enough that the caller's own deadline stays responsive, long enough that a
/// real reply is caught in one park.
const PARK_SLICE_NS: u64 = 100_000_000; // 100 ms

// errno values the bridge returns (asm-generic, shared by all three ISAs).
const EAGAIN: i64 = -11;
const EMFILE: i64 = -24;
const EINVAL: i64 = -22;
const ENETUNREACH: i64 = -101;
const ECONNREFUSED: i64 = -111;
const ETIMEDOUT: i64 = -110;
const EHOSTUNREACH: i64 = -113;
const ENOTCONN: i64 = -107;

// ------------------------------------------------------------------- the state

/// One buffered inbound datagram.
#[derive(Copy, Clone)]
struct Dgram {
    src_ip: [u8; 4],
    src_port: u16,
    len: u16,
    buf: [u8; UDP_MAX],
}

impl Dgram {
    const fn new() -> Dgram {
        Dgram {
            src_ip: [0; 4],
            src_port: 0,
            len: 0,
            buf: [0; UDP_MAX],
        }
    }
}

/// A bound remote UDP endpoint: a local port plus a small inbound queue.
struct UdpEp {
    used: bool,
    port: u16,
    q: [Dgram; UDP_QUEUE],
    qhead: usize,
    qlen: usize,
}

impl UdpEp {
    const fn new() -> UdpEp {
        UdpEp {
            used: false,
            port: 0,
            q: [const { Dgram::new() }; UDP_QUEUE],
            qhead: 0,
            qlen: 0,
        }
    }
}

/// A remote TCP connection: the `rheo-net` state machine plus its peer identity.
struct TcpConn {
    conn: Option<Connection<FixedWindow>>,
    peer_ip: [u8; 4],
    peer_port: u16,
    local_port: u16,
    /// The peer's MAC (resolved once at connect; a connection does not re-ARP).
    peer_mac: Mac,
}

impl TcpConn {
    const fn new() -> TcpConn {
        TcpConn {
            conn: None,
            peer_ip: [0; 4],
            peer_port: 0,
            local_port: 0,
            peer_mac: Mac([0; 6]),
        }
    }
}

static mut OUR_MAC: Mac = Mac([0; 6]);
static mut ARP_CACHE: [Option<([u8; 4], Mac)>; ARP_ENTRIES] = [None; ARP_ENTRIES];
static mut UDP_TBL: [UdpEp; UDP_EPS] = [const { UdpEp::new() }; UDP_EPS];
static mut TCP_TBL: [TcpConn; TCP_CONNS] = [const { TcpConn::new() }; TCP_CONNS];
/// A scratch frame buffer. The datapath is single-CPU and re-entered only from a
/// cell's (serialised) trap, so one buffer serves receive and transmit framing.
static mut FRAME: [u8; wire::MAX_FRAME] = [0; wire::MAX_FRAME];

// SAFETY (all the accessors below): the kernel is single-CPU and these handlers
// run only inside a cell's synchronous syscall trap, so no two of them can be
// live at once - the same discipline `linux::pipe` / `linux::inetsock` use for
// their own fixed statics.
fn udps() -> &'static mut [UdpEp; UDP_EPS] {
    unsafe { &mut *addr_of_mut!(UDP_TBL) }
}
fn tcps() -> &'static mut [TcpConn; TCP_CONNS] {
    unsafe { &mut *addr_of_mut!(TCP_TBL) }
}
fn arps() -> &'static mut [Option<([u8; 4], Mac)>; ARP_ENTRIES] {
    unsafe { &mut *addr_of_mut!(ARP_CACHE) }
}
fn our_mac() -> Mac {
    unsafe { *addr_of_mut!(OUR_MAC) }
}
fn frame_buf() -> &'static mut [u8; wire::MAX_FRAME] {
    unsafe { &mut *addr_of_mut!(FRAME) }
}

/// Install the datapath: read the NIC's MAC and clear every registry. Call once,
/// after `virtio_net::install`, before registering [`ops`]. Returns false if no NIC
/// is present (the personality then keeps answering `ENETUNREACH`, honestly).
pub fn init() -> bool {
    let Some(m) = virtio_net::mac_addr() else {
        return false;
    };
    // SAFETY: single-threaded boot, before any cell runs.
    unsafe {
        *addr_of_mut!(OUR_MAC) = Mac(m);
        *addr_of_mut!(ARP_CACHE) = [None; ARP_ENTRIES];
    }
    for e in udps().iter_mut() {
        *e = UdpEp::new();
    }
    for c in tcps().iter_mut() {
        *c = TcpConn::new();
    }
    true
}

/// The `svc::SocketOps` table to register with `svc::set_socket_ops`.
pub fn ops() -> SocketOps {
    SocketOps {
        local_ip: op_local_ip,
        udp_bind: op_udp_bind,
        udp_close: op_udp_close,
        udp_send: op_udp_send,
        udp_recv: op_udp_recv,
        udp_pending: op_udp_pending,
        tcp_connect: op_tcp_connect,
        tcp_send: op_tcp_send,
        tcp_recv: op_tcp_recv,
        tcp_pending: op_tcp_pending,
        tcp_close: op_tcp_close,
    }
}

// ------------------------------------------------------------ time + next hop

/// The monotonic clock in nanoseconds - what `tcp::Connection` expects for its
/// RTO/RTT arithmetic.
fn now_ns() -> u64 {
    arch::ticks_to_ns(arch::cycles())
}

/// The next hop for `dst`: the destination itself when it is on our own /24, else
/// the gateway - the routing decision a real host makes.
fn next_hop(dst: [u8; 4]) -> [u8; 4] {
    let l = LOCAL_IP.octets();
    if dst[0] == l[0] && dst[1] == l[1] && dst[2] == l[2] {
        dst
    } else {
        GATEWAY_IP.octets()
    }
}

fn arp_lookup(ip: [u8; 4]) -> Option<Mac> {
    arps()
        .iter()
        .flatten()
        .find(|(a, _)| *a == ip)
        .map(|(_, m)| *m)
}

fn arp_insert(ip: [u8; 4], mac: Mac) {
    let t = arps();
    if let Some(slot) = t.iter().position(|e| matches!(e, Some((a, _)) if *a == ip)) {
        t[slot] = Some((ip, mac));
        return;
    }
    // Free slot, else evict slot 0 (a 4-entry cache under one datapath).
    let slot = t.iter().position(|e| e.is_none()).unwrap_or(0);
    t[slot] = Some((ip, mac));
}

/// Resolve the L2 address for `dst`'s next hop, ARPing for it if it is not cached.
/// Bounded: the request is retried a few times, each retry parking on the wire, so
/// a silent link fails with `EHOSTUNREACH` rather than spinning forever.
fn resolve(dst: [u8; 4]) -> Result<Mac, i64> {
    let hop = next_hop(dst);
    if let Some(m) = arp_lookup(hop) {
        return Ok(m);
    }
    let req = arp::build_request(our_mac(), LOCAL_IP, Ipv4Addr(hop));
    for _ in 0..4 {
        if !virtio_net::send_frame_slice(&req) {
            return Err(ENETUNREACH);
        }
        // Park for the reply. Anything else that lands is demultiplexed, never
        // dropped (a UDP reply can overtake an ARP reply).
        for _ in 0..8 {
            if !pump_once(PARK_SLICE_NS) {
                break;
            }
            if let Some(m) = arp_lookup(hop) {
                return Ok(m);
            }
        }
    }
    Err(EHOSTUNREACH)
}

// -------------------------------------------------------------- receive + demux

/// Park for one inbound frame (up to `timeout_ns`) and demultiplex it. Returns
/// true if a frame was received (whether or not it was for anyone).
fn pump_once(timeout_ns: u64) -> bool {
    let buf = frame_buf();
    let n = net_rx::wait_frame_slice(buf, timeout_ns);
    if n == 0 {
        return false;
    }
    demux(n);
    true
}

/// Drain whatever is already queued on the NIC without blocking.
fn pump_nonblocking() {
    for _ in 0..16 {
        let buf = frame_buf();
        match virtio_net::recv_frame_slice(buf) {
            Some(n) if n > 0 => demux(n),
            _ => return,
        }
    }
}

/// Route one received frame (the first `n` bytes of the scratch buffer) to the ARP
/// cache, a UDP endpoint queue, or a TCP connection. Anything else is dropped.
fn demux(n: usize) {
    // Copy the frame out of the shared scratch buffer first: the UDP/TCP paths
    // below build and send frames, which would otherwise overwrite it.
    let mut local = [0u8; wire::MAX_FRAME];
    let n = n.min(wire::MAX_FRAME);
    local[..n].copy_from_slice(&frame_buf()[..n]);
    let frame = &local[..n];

    let Some(ef) = eth::Frame::parse(frame) else {
        return;
    };
    match ef.ethertype() {
        eth::ethertype::ARP => {
            if let Some(pkt) = arp::ArpPacket::parse(ef.payload())
                && pkt.oper == arp::OP_REPLY
            {
                arp_insert(pkt.spa.octets(), pkt.sha);
            }
        }
        eth::ethertype::IPV4 => {
            let Some(parsed) = wire::parse_ipv4(frame) else {
                return;
            };
            let (start, end) = parsed.l4;
            if end <= start || end > n {
                return;
            }
            let l4 = &frame[start..end];
            match parsed.header.protocol {
                ip::proto::UDP => deliver_udp(parsed.header.src, parsed.header.dst, l4),
                ip::proto::TCP => deliver_tcp(parsed.header.src, l4),
                _ => {}
            }
        }
        _ => {}
    }
}

/// Verify and queue an inbound UDP datagram against its bound local port.
fn deliver_udp(src: Ipv4Addr, dst: Ipv4Addr, datagram: &[u8]) {
    let Some(hdr) = udp::UdpHeader::parse(datagram) else {
        return;
    };
    if !udp::verify_checksum_v4(src, dst, datagram) {
        return;
    }
    let Some(payload) = hdr.payload(datagram) else {
        return;
    };
    let Some(idx) = udps().iter().position(|e| e.used && e.port == hdr.dst_port) else {
        return;
    };
    let e = &mut udps()[idx];
    if e.qlen >= UDP_QUEUE {
        return; // full: dropped, as UDP permits
    }
    let slot = (e.qhead + e.qlen) % UDP_QUEUE;
    let len = payload.len().min(UDP_MAX);
    let d = &mut e.q[slot];
    d.src_ip = src.octets();
    d.src_port = hdr.src_port;
    d.len = len as u16;
    d.buf[..len].copy_from_slice(&payload[..len]);
    e.qlen += 1;
}

/// Feed an inbound TCP segment to the connection its four-tuple selects, then push
/// out whatever the state machine wants to send in reply (an ACK, a retransmit).
fn deliver_tcp(src: Ipv4Addr, seg: &[u8]) {
    let Some(parsed) = tcp::Segment::decode(seg) else {
        return;
    };
    let Some(idx) = tcps().iter().position(|c| {
        c.conn.is_some()
            && c.peer_ip == src.octets()
            && c.peer_port == parsed.src_port
            && c.local_port == parsed.dst_port
    }) else {
        return;
    };
    let now = now_ns();
    let (peer_ip, peer_mac) = {
        let c = &tcps()[idx];
        (c.peer_ip, c.peer_mac)
    };
    if let Some(conn) = tcps()[idx].conn.as_mut() {
        conn.on_wire_segment(now, seg);
    }
    drain_tcp_tx(idx, peer_ip, peer_mac);
}

/// Send every segment connection `idx` currently wants to transmit.
fn drain_tcp_tx(idx: usize, peer_ip: [u8; 4], peer_mac: Mac) {
    loop {
        let now = now_ns();
        let Some(conn) = tcps()[idx].conn.as_mut() else {
            return;
        };
        let Some(seg) = conn.poll(now) else {
            return;
        };
        send_ipv4(peer_mac, peer_ip, ip::proto::TCP, &seg);
    }
}

// ------------------------------------------------------------------- transmit

/// Frame `l4` as IPv4 (through the stack's own `wire`/`ip` builders) and hand the
/// frame to the NIC. Returns false if framing or the device failed.
fn send_ipv4(dst_mac: Mac, dst_ip: [u8; 4], protocol: u8, l4: &[u8]) -> bool {
    let framing = wire::Ipv4Framing {
        dst_mac,
        src_mac: our_mac(),
        ttl: ip::DEFAULT_TTL,
        protocol,
        src_ip: LOCAL_IP,
        dst_ip: Ipv4Addr(dst_ip),
    };
    let mut out = [0u8; wire::MAX_FRAME];
    match wire::frame_ipv4(&framing, l4, &mut out) {
        Ok(len) => virtio_net::send_frame_slice(&out[..len]),
        Err(_) => false,
    }
}

// ----------------------------------------------------------------- UDP handlers

fn op_local_ip() -> [u8; 4] {
    LOCAL_IP.octets()
}

fn op_udp_bind(port: u16) -> i64 {
    if port == 0 {
        return EINVAL;
    }
    let t = udps();
    if t.iter().any(|e| e.used && e.port == port) {
        return EINVAL; // the personality allocates ports; a clash is a bug
    }
    let Some(idx) = t.iter().position(|e| !e.used) else {
        return EMFILE;
    };
    t[idx] = UdpEp::new();
    t[idx].used = true;
    t[idx].port = port;
    idx as i64
}

fn op_udp_close(ep: u64) {
    if let Some(e) = udps().get_mut(ep as usize) {
        *e = UdpEp::new();
    }
}

fn op_udp_send(ep: u64, dst_ip: [u8; 4], dst_port: u16, buf_va: u64, len: u64) -> i64 {
    let Some(e) = udps().get(ep as usize) else {
        return EINVAL;
    };
    if !e.used {
        return EINVAL;
    }
    let src_port = e.port;
    let n = (len as usize).min(UDP_MAX);
    // SAFETY: the calling cell's address space is active during its trap, so
    // `buf_va` is `n` readable bytes there (the `FileOps` convention).
    let payload = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n) };
    let mac = match resolve(dst_ip) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let mut datagram = [0u8; UDP_MAX + udp::HEADER_LEN];
    let Some(dlen) = udp::build_v4(
        LOCAL_IP,
        Ipv4Addr(dst_ip),
        src_port,
        dst_port,
        payload,
        &mut datagram,
    ) else {
        return EINVAL;
    };
    if send_ipv4(mac, dst_ip, ip::proto::UDP, &datagram[..dlen]) {
        len as i64
    } else {
        ENETUNREACH
    }
}

fn op_udp_recv(
    ep: u64,
    buf_va: u64,
    len: u64,
    src_ip_va: u64,
    src_port_va: u64,
    timeout_ns: u64,
) -> i64 {
    let idx = ep as usize;
    if udps().get(idx).is_none_or(|e| !e.used) {
        return EINVAL;
    }
    pump_nonblocking();
    let deadline = now_ns().saturating_add(timeout_ns);
    while udps()[idx].qlen == 0 {
        let now = now_ns();
        if timeout_ns == 0 || now >= deadline {
            return EAGAIN;
        }
        let slice = (deadline - now).min(PARK_SLICE_NS);
        pump_once(slice);
    }
    let want = len as usize;
    let e = &mut udps()[idx];
    let d = e.q[e.qhead];
    e.qhead = (e.qhead + 1) % UDP_QUEUE;
    e.qlen -= 1;
    let n = (d.len as usize).min(want);
    // SAFETY: `buf_va` is `len` writable bytes in the active cell; the out-params
    // are a 4-byte IPv4 and a `u16` the personality supplied.
    unsafe {
        core::ptr::copy_nonoverlapping(d.buf.as_ptr(), buf_va as *mut u8, n);
        if src_ip_va != 0 {
            core::ptr::copy_nonoverlapping(d.src_ip.as_ptr(), src_ip_va as *mut u8, 4);
        }
        if src_port_va != 0 {
            (src_port_va as *mut u16).write(d.src_port);
        }
    }
    n as i64
}

fn op_udp_pending(ep: u64) -> bool {
    pump_nonblocking();
    udps().get(ep as usize).is_some_and(|e| e.qlen > 0)
}

// ----------------------------------------------------------------- TCP handlers

fn op_tcp_connect(dst_ip: [u8; 4], dst_port: u16, src_port: u16, timeout_ns: u64) -> i64 {
    let Some(idx) = tcps().iter().position(|c| c.conn.is_none()) else {
        return EMFILE;
    };
    let mac = match resolve(dst_ip) {
        Ok(m) => m,
        Err(e) => return e,
    };
    // The initial send sequence comes from the kernel's per-cell DRBG - a
    // non-secret value, exactly the randomness class `rng` is for.
    let iss = kernel::rng::derive_cell_drbg().next_u64() as u32;
    let c = &mut tcps()[idx];
    *c = TcpConn::new();
    c.peer_ip = dst_ip;
    c.peer_port = dst_port;
    c.local_port = src_port;
    c.peer_mac = mac;
    c.conn = Some(Connection::connect(
        LOCAL_IP,
        src_port,
        Ipv4Addr(dst_ip),
        dst_port,
        iss,
    ));

    let deadline = now_ns().saturating_add(timeout_ns);
    loop {
        drain_tcp_tx(idx, dst_ip, mac);
        let state = tcps()[idx].conn.as_ref().map(|c| c.state());
        match state {
            Some(State::Established) => return idx as i64,
            // A RST (or any drop to CLOSED) before the handshake completed is a
            // refused connection - the classic remote-TCP failure.
            Some(State::Closed) => {
                tcps()[idx] = TcpConn::new();
                return ECONNREFUSED;
            }
            None => return ECONNREFUSED,
            _ => {}
        }
        let now = now_ns();
        if now >= deadline {
            tcps()[idx] = TcpConn::new();
            return ETIMEDOUT;
        }
        pump_once((deadline - now).min(PARK_SLICE_NS));
    }
}

fn op_tcp_send(h: u64, buf_va: u64, len: u64) -> i64 {
    let idx = h as usize;
    let Some(c) = tcps().get(idx) else {
        return EINVAL;
    };
    if c.conn.is_none() {
        return ENOTCONN;
    }
    let (peer_ip, peer_mac) = (c.peer_ip, c.peer_mac);
    let n = len as usize;
    // **Process inbound segments before accepting more data.** A TCP send queue is
    // freed by the peer's ACKs, and ACKs only reach the state machine through
    // `on_wire_segment` - which nothing called on this path. So `snd_una` never
    // advanced, the send queue filled, `conn.write` accepted 0 and the write
    // reported EAGAIN forever: any body larger than the send queue **deadlocked**.
    // One drain of whatever the NIC already has is enough (it is where the ACKs
    // are), and it costs nothing when the queue is empty.
    pump_nonblocking();
    // SAFETY: `buf_va` is `n` readable bytes in the active cell.
    let data = unsafe { core::slice::from_raw_parts(buf_va as *const u8, n) };
    let accepted = match tcps()[idx].conn.as_mut() {
        Some(conn) => conn.write(data),
        None => return ENOTCONN,
    };
    drain_tcp_tx(idx, peer_ip, peer_mac);
    if accepted == 0 && n > 0 {
        EAGAIN
    } else {
        accepted as i64
    }
}

fn op_tcp_recv(h: u64, buf_va: u64, len: u64, timeout_ns: u64) -> i64 {
    let idx = h as usize;
    let Some(c) = tcps().get(idx) else {
        return EINVAL;
    };
    if c.conn.is_none() {
        return ENOTCONN;
    }
    let (peer_ip, peer_mac) = (c.peer_ip, c.peer_mac);
    pump_nonblocking();
    let deadline = now_ns().saturating_add(timeout_ns);
    loop {
        drain_tcp_tx(idx, peer_ip, peer_mac);
        let (avail, closed) = match tcps()[idx].conn.as_ref() {
            Some(conn) => (
                conn.recv_available(),
                matches!(
                    conn.state(),
                    State::Closed | State::CloseWait | State::TimeWait | State::LastAck
                ),
            ),
            None => return ENOTCONN,
        };
        if avail > 0 {
            break;
        }
        if closed {
            return 0; // orderly EOF (or a reset peer): no more data will arrive
        }
        let now = now_ns();
        if timeout_ns == 0 || now >= deadline {
            return EAGAIN;
        }
        pump_once((deadline - now).min(PARK_SLICE_NS));
    }
    let want = len as usize;
    let mut tmp = [0u8; UDP_MAX];
    let cap = want.min(tmp.len());
    let n = match tcps()[idx].conn.as_mut() {
        Some(conn) => conn.read(&mut tmp[..cap]),
        None => return ENOTCONN,
    };
    // SAFETY: `buf_va` is `len` writable bytes in the active cell.
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf_va as *mut u8, n);
    }
    drain_tcp_tx(idx, peer_ip, peer_mac);
    n as i64
}

/// `poll`/`epoll` readiness for a connected remote TCP socket
/// (docs/ARCHITECTURE-DEBT.md 2.4). Pumps the receive path first (so it reports what
/// has actually arrived, not what arrived last time someone asked), then answers
/// "readable" for queued bytes **or** a closed peer - EOF is a readable condition.
/// Before this existed the personality had no way to ask, and hardcoded `true`.
fn op_tcp_pending(h: u64) -> bool {
    let idx = h as usize;
    let Some(c) = tcps().get(idx) else {
        return false;
    };
    if c.conn.is_none() {
        return false;
    }
    let (peer_ip, peer_mac) = (c.peer_ip, c.peer_mac);
    pump_nonblocking();
    drain_tcp_tx(idx, peer_ip, peer_mac);
    match tcps()[idx].conn.as_ref() {
        Some(conn) => {
            conn.recv_available() > 0
                || matches!(
                    conn.state(),
                    State::Closed | State::CloseWait | State::TimeWait | State::LastAck
                )
        }
        None => false,
    }
}

fn op_tcp_close(h: u64) {
    let idx = h as usize;
    let Some(c) = tcps().get(idx) else {
        return;
    };
    let (peer_ip, peer_mac) = (c.peer_ip, c.peer_mac);
    if let Some(conn) = tcps()[idx].conn.as_mut() {
        conn.close();
    }
    // Push the FIN out; the peer's ACK/FIN is not waited for (no lingering close).
    drain_tcp_tx(idx, peer_ip, peer_mac);
    tcps()[idx] = TcpConn::new();
}
