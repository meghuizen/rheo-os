//! Shared L2/L3 send path (docs/NETSTACK.md): frame an IPv4 packet (Ethernet +
//! IPv4 header + an L4 payload), parse a received IPv4 frame, and resolve the
//! next-hop MAC. `udp` and `icmp` share this so the eth/ip framing, the ARP
//! resolution, and the **IPv4 TTL hook** (settable per send, the seam a later
//! traceroute increments) live in exactly one place - no per-protocol copy.

use crate::arp::{self, ArpCache, ResolveError};
use crate::eth::{self, Mac};
use crate::ip::{self, Ipv4Addr, Ipv4Header};
use librheo::net;

/// The default IPv4 TTL for a locally-originated packet (RFC 1122 suggests 64).
pub const DEFAULT_TTL: u8 = 64;

/// The largest frame `wire` builds or receives (standard Ethernet MTU + headers;
/// jumbo frames are a later phase).
pub const MAX_FRAME: usize = 1600;

/// The offset of the L4 payload inside a built/received IPv4-over-Ethernet frame
/// (14-byte Ethernet header + 20-byte IPv4 header, no options).
pub const L4_OFFSET: usize = eth::HEADER_LEN + ip::IPV4_HEADER_LEN;

/// A failure in the shared wire path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The raw-frame TX/RX path (`librheo::net`) failed.
    Net,
    /// No ARP reply for the next hop within the retry budget.
    ArpTimeout,
    /// The caller's buffer could not hold the frame/payload.
    TooBig,
}

impl From<ResolveError> for WireError {
    fn from(e: ResolveError) -> WireError {
        match e {
            ResolveError::Net => WireError::Net,
            ResolveError::Timeout => WireError::ArpTimeout,
        }
    }
}

/// Resolve the next-hop MAC for `dst_ip`. SLIRP proxy-ARPs the whole `10.0.2.0/24`
/// guest subnet, so for the deterministic proof we ARP the destination address
/// directly (the reply carries SLIRP's gateway MAC either way). A real deployment
/// would ARP the gateway for an off-link destination; that routing decision is an
/// N1c refinement (host config + a routing table), documented in docs/NETSTACK.md.
pub async fn resolve_next_hop(
    cache: &mut ArpCache,
    src_mac: Mac,
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
) -> Result<Mac, WireError> {
    Ok(arp::resolve(cache, src_mac, src_ip, dst_ip).await?)
}

/// The addressing/framing parameters for one IPv4-over-Ethernet packet: the L2
/// MACs, the IPv4 TTL (the traceroute hook), the L4 protocol, and the IPv4
/// endpoints. Grouped into one struct so [`frame_ipv4`] stays a small call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv4Framing {
    pub dst_mac: Mac,
    pub src_mac: Mac,
    pub ttl: u8,
    pub protocol: u8,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
}

/// Frame an IPv4 packet: Ethernet header, then a 20-byte IPv4 header (with the
/// TTL the caller chose and a correct header checksum), then `l4` verbatim. `l4`
/// is the already-built transport datagram (a UDP datagram, an ICMP message).
/// Returns the total frame length, or `WireError::TooBig` if `out` is too small.
pub fn frame_ipv4(f: &Ipv4Framing, l4: &[u8], out: &mut [u8]) -> Result<usize, WireError> {
    let total = L4_OFFSET + l4.len();
    if out.len() < total {
        return Err(WireError::TooBig);
    }
    let eth_hdr = eth::Header {
        dst: f.dst_mac,
        src: f.src_mac,
        ethertype: eth::ethertype::IPV4,
    };
    eth_hdr.write(&mut out[..eth::HEADER_LEN]).unwrap();
    let ip_hdr = Ipv4Header {
        dscp_ecn: 0,
        total_len: (ip::IPV4_HEADER_LEN + l4.len()) as u16,
        identification: 0,
        flags_frag: 0x4000, // Don't Fragment
        ttl: f.ttl,
        protocol: f.protocol,
        src: f.src_ip,
        dst: f.dst_ip,
    };
    ip_hdr
        .write(&mut out[eth::HEADER_LEN..L4_OFFSET])
        .ok_or(WireError::TooBig)?;
    out[L4_OFFSET..total].copy_from_slice(l4);
    Ok(total)
}

/// A parsed IPv4-over-Ethernet frame: the IPv4 header and the byte range of its
/// L4 payload within the original frame buffer.
pub struct Ipv4Frame {
    pub header: Ipv4Header,
    /// `[start, end)` of the L4 payload in the frame buffer.
    pub l4: (usize, usize),
}

/// Parse a received Ethernet frame as IPv4. Returns `None` unless it is a
/// well-formed IPv4-over-Ethernet frame with a valid header checksum. The L4
/// payload length comes from the IPv4 `total_len` (clamped to the frame).
pub fn parse_ipv4(frame: &[u8]) -> Option<Ipv4Frame> {
    let eth_frame = eth::Frame::parse(frame)?;
    if eth_frame.ethertype() != eth::ethertype::IPV4 {
        return None;
    }
    let ip_bytes = &frame[eth::HEADER_LEN..];
    if !Ipv4Header::verify_checksum(ip_bytes) {
        return None;
    }
    let header = Ipv4Header::parse(ip_bytes)?;
    let start = L4_OFFSET;
    let end = core::cmp::min(eth::HEADER_LEN + header.total_len as usize, frame.len());
    if end < start {
        return None;
    }
    Some(Ipv4Frame {
        header,
        l4: (start, end),
    })
}

/// Send one already-framed raw Ethernet frame over `librheo::net`.
pub async fn send_frame(frame: &[u8]) -> Result<(), WireError> {
    net::send(frame)
        .await
        .map(|_| ())
        .map_err(|_| WireError::Net)
}

/// Poll for one raw Ethernet frame into `buf` (a single `net::recv`). Returns the
/// frame length (`0` if nothing is ready - the caller re-polls).
pub async fn recv_frame(buf: &mut [u8]) -> Result<usize, WireError> {
    net::recv(buf).await.map_err(|_| WireError::Net)
}
