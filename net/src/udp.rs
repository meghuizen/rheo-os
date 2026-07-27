//! UDP datagrams (docs/NETSTACK.md L4). A UDP header is 8 bytes -
//! `[src port 2][dst port 2][length 2][checksum 2]` (all big-endian) - then the
//! payload. `length` counts the header **and** payload.
//!
//! ## The checksum (correctness-critical)
//! UDP's checksum covers a **pseudo-header** (source + destination address,
//! protocol, and the UDP length) followed by the UDP header (its own checksum
//! field zeroed) and the payload. This is exactly what the N1a [`Checksum`]
//! accumulator was built for: it is fed several slices - the pseudo-header, the
//! header, the payload - and carries a leftover odd byte across `add` calls, so
//! summing them separately matches one contiguous buffer. We reuse it unchanged
//! (no new checksum code, the same scalar oracle).
//!
//! Two pseudo-headers exist, one per IP version:
//! - **IPv4**: `[src 4][dst 4][zero 1][proto 1][udp_len 2]` (12 bytes).
//! - **IPv6**: `[src 16][dst 16][udp_len 4][zero 3][next_header 1]` (40 bytes).
//!
//! Over IPv4 the checksum is optional (a zero field means "not computed"); over
//! **IPv6 it is mandatory** (RFC 8200). A computed checksum of `0x0000` is
//! transmitted as `0xFFFF` (both fold to the same ones-complement value, and
//! `0x0000` is reserved to mean "absent").

#[cfg(feature = "hosted")]
use crate::arp::ArpCache;
#[cfg(feature = "hosted")]
use crate::eth::Mac;
use crate::ip::{self, Checksum, Ipv4Addr, Ipv6Addr};
#[cfg(feature = "hosted")]
use crate::wire::{self, WireError};

/// The UDP header length in bytes.
pub const HEADER_LEN: usize = 8;

/// The RX poll budget for [`UdpEndpoint::recv_from`] (same rationale as
/// `arp::RESOLVE_RETRIES`: each `recv` is a doorbell that lets SLIRP run, so a
/// reply lands within a handful of iterations; the cap only guards a wholly
/// non-delivering backend). A wall-clock `librheo::time::timeout` is the option
/// for a real deployment.
#[cfg(feature = "hosted")]
pub const RECV_RETRIES: u32 = 200_000;

/// How many datagrams [`UdpEndpoint::recv_from_timeout`] looks at inside its deadline
/// before giving up - a **frame** count, so a chatty link cannot stretch a bounded
/// wait, while the wait itself stays a duration.
#[cfg(feature = "hosted")]
pub const RECV_FRAME_BUDGET: u32 = 32;

/// A parsed UDP header (the 8 bytes before the payload).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UdpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    /// Datagram length: header (8) + payload.
    pub length: u16,
    /// The on-wire checksum field (0 over IPv4 means "not computed").
    pub checksum: u16,
}

impl UdpHeader {
    /// Parse the 8-byte UDP header from the front of `buf`. Returns `None` if
    /// `buf` is shorter than the header.
    pub fn parse(buf: &[u8]) -> Option<UdpHeader> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(UdpHeader {
            src_port: u16::from_be_bytes([buf[0], buf[1]]),
            dst_port: u16::from_be_bytes([buf[2], buf[3]]),
            length: u16::from_be_bytes([buf[4], buf[5]]),
            checksum: u16::from_be_bytes([buf[6], buf[7]]),
        })
    }

    /// The payload slice after the header, per the header's `length` field
    /// (clamped to what `buf` actually holds). Returns `None` if `length` is
    /// smaller than the 8-byte header.
    pub fn payload<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let len = self.length as usize;
        if len < HEADER_LEN {
            return None;
        }
        let end = core::cmp::min(len, buf.len());
        Some(&buf[HEADER_LEN..end])
    }
}

/// Fold a UDP header (with its checksum field zeroed) plus the payload into an
/// already-primed [`Checksum`] that holds the pseudo-header. Shared by the v4
/// and v6 paths so the header/payload folding is written once.
fn add_header_and_payload(
    c: &mut Checksum,
    src_port: u16,
    dst_port: u16,
    udp_len: u16,
    payload: &[u8],
) {
    c.add(&src_port.to_be_bytes());
    c.add(&dst_port.to_be_bytes());
    c.add(&udp_len.to_be_bytes());
    c.add(&[0, 0]); // checksum field zero during computation
    c.add(payload);
}

/// Fold a computed sum's `0x0000` to the transmitted `0xFFFF` (RFC 768: a zero
/// field means "no checksum", so a genuine zero result is sent as all-ones).
fn nonzero(ck: u16) -> u16 {
    if ck == 0 { 0xFFFF } else { ck }
}

/// The UDP checksum over an **IPv4** pseudo-header + header + `payload`.
pub fn checksum_v4(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> u16 {
    let udp_len = (HEADER_LEN + payload.len()) as u16;
    let mut c = Checksum::new();
    // IPv4 pseudo-header: src, dst, zero, protocol, UDP length.
    c.add(&src.0);
    c.add(&dst.0);
    c.add(&[0, ip::proto::UDP]);
    c.add(&udp_len.to_be_bytes());
    add_header_and_payload(&mut c, src_port, dst_port, udp_len, payload);
    nonzero(c.finish())
}

/// The UDP checksum over an **IPv6** pseudo-header + header + `payload`. IPv6
/// mandates the checksum (RFC 8200), so this is never optional.
pub fn checksum_v6(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> u16 {
    let udp_len = (HEADER_LEN + payload.len()) as u32;
    let mut c = Checksum::new();
    // IPv6 pseudo-header: src, dst, upper-layer length (4), zero (3), next header.
    c.add(&src.0);
    c.add(&dst.0);
    c.add(&udp_len.to_be_bytes());
    c.add(&[0, 0, 0, ip::proto::UDP]);
    add_header_and_payload(&mut c, src_port, dst_port, udp_len as u16, payload);
    nonzero(c.finish())
}

/// Write a full UDP datagram (header + `payload`) into `out`, checksummed for
/// **IPv4**. Returns the total length (`8 + payload.len()`), or `None` if `out`
/// is too small.
pub fn build_v4(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return None;
    }
    let udp_len = total as u16;
    let ck = checksum_v4(src, dst, src_port, dst_port, payload);
    write_datagram(src_port, dst_port, udp_len, ck, payload, out);
    Some(total)
}

/// Write a full UDP datagram (header + `payload`) into `out`, checksummed for
/// **IPv6** (mandatory). Returns the total length, or `None` if `out` is too
/// small.
pub fn build_v6(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return None;
    }
    let udp_len = total as u16;
    let ck = checksum_v6(src, dst, src_port, dst_port, payload);
    write_datagram(src_port, dst_port, udp_len, ck, payload, out);
    Some(total)
}

/// Lay down the 8-byte header + payload (checksum already computed).
fn write_datagram(
    src_port: u16,
    dst_port: u16,
    udp_len: u16,
    checksum: u16,
    payload: &[u8],
    out: &mut [u8],
) {
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..6].copy_from_slice(&udp_len.to_be_bytes());
    out[6..8].copy_from_slice(&checksum.to_be_bytes());
    out[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
}

/// Verify a received UDP-over-IPv4 datagram's checksum. `datagram` is the UDP
/// header + payload as received. A transmitted checksum field of `0` means the
/// sender did not compute one (legal over IPv4), so this returns `true`.
pub fn verify_checksum_v4(src: Ipv4Addr, dst: Ipv4Addr, datagram: &[u8]) -> bool {
    let Some(hdr) = UdpHeader::parse(datagram) else {
        return false;
    };
    if hdr.checksum == 0 {
        return true; // not computed by the sender (IPv4 only)
    }
    // Sum the pseudo-header + the whole datagram as received (checksum field
    // included); a correct datagram folds to zero.
    let udp_len = hdr.length;
    let mut c = Checksum::new();
    c.add(&src.0);
    c.add(&dst.0);
    c.add(&[0, ip::proto::UDP]);
    c.add(&udp_len.to_be_bytes());
    let end = core::cmp::min(udp_len as usize, datagram.len());
    c.add(&datagram[..end]);
    c.finish() == 0
}

/// A datagram received by [`UdpEndpoint::recv_from`]: who it came from and how
/// much of the caller's buffer holds the payload.
#[cfg(feature = "hosted")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub src_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    /// Bytes of payload written to the caller's buffer.
    pub len: usize,
}

/// An async UDP-over-IPv4 endpoint (docs/NETSTACK.md L4). It owns the local
/// identity (MAC + IPv4 address), an [`ArpCache`] for next-hop resolution, and
/// the IPv4 TTL to stamp on sent packets. Datagrams go out through
/// `net::udp` -> `net::wire` (eth/ip framing) -> `librheo::net`, resolving the
/// next hop via `net::arp`.
#[cfg(feature = "hosted")]
pub struct UdpEndpoint {
    src_mac: Mac,
    src_ip: Ipv4Addr,
    ttl: u8,
    cache: ArpCache,
    /// The gateway for off-link destinations, when the endpoint was built from a
    /// [`HostConfig`](crate::hostcfg::HostConfig) that had one. `None` means "treat
    /// every destination as on-link", which is the pre-N4c behaviour and is what
    /// SLIRP's proxy-ARP actually wants.
    gateway: Option<Ipv4Addr>,
    /// The netmask, when known - together with `src_ip` this is what makes
    /// on-link-vs-gateway a real decision rather than a guess.
    netmask: Option<Ipv4Addr>,
}

#[cfg(feature = "hosted")]
impl UdpEndpoint {
    /// A new endpoint for `src_ip` reachable at `src_mac`, TTL defaulting to
    /// [`wire::DEFAULT_TTL`]. No netmask or gateway, so every destination is
    /// treated as on-link (see [`from_host_config`](Self::from_host_config) for the
    /// routed form).
    pub fn new(src_mac: Mac, src_ip: Ipv4Addr) -> UdpEndpoint {
        UdpEndpoint {
            src_mac,
            src_ip,
            ttl: wire::DEFAULT_TTL,
            cache: ArpCache::new(),
            gateway: None,
            netmask: None,
        }
    }

    /// An endpoint whose identity **and routing** come from the host-config store
    /// (docs/NETSTACK.md §20, rheo-net N4c): the source address, the netmask, and
    /// the default gateway.
    ///
    /// This is the difference that matters: [`send_to`](Self::send_to) resolves the
    /// **next hop** rather than the destination, so an off-link destination is sent
    /// to the gateway's MAC - the routing decision `net::wire` used to list as a
    /// deferred refinement. With [`UdpEndpoint::new`] there is no netmask, so the
    /// old behaviour (ARP the destination directly) is unchanged.
    pub fn from_host_config(src_mac: Mac, cfg: &crate::hostcfg::HostConfig) -> UdpEndpoint {
        UdpEndpoint {
            src_mac,
            src_ip: cfg.source_address(),
            ttl: wire::DEFAULT_TTL,
            cache: ArpCache::new(),
            gateway: cfg.gateway(),
            netmask: cfg.netmask(),
        }
    }

    /// The source address this endpoint sends from.
    pub fn src_ip(&self) -> Ipv4Addr {
        self.src_ip
    }

    /// The gateway used for off-link destinations, if any.
    pub fn gateway(&self) -> Option<Ipv4Addr> {
        self.gateway
    }

    /// The address whose MAC must be resolved to reach `dst`: `dst` itself when it is
    /// on-link (or when no netmask is configured), the gateway otherwise. Exposed so
    /// the routing decision is testable without sending a packet.
    pub fn next_hop(&self, dst: Ipv4Addr) -> Ipv4Addr {
        let (Some(mask), Some(gw)) = (self.netmask, self.gateway) else {
            return dst;
        };
        let m = u32::from_be_bytes(mask.0);
        let on_link = (u32::from_be_bytes(self.src_ip.0) & m) == (u32::from_be_bytes(dst.0) & m);
        if on_link { dst } else { gw }
    }

    /// The IPv4 TTL stamped on sent packets (the traceroute hook - a later phase
    /// increments this and reads the ICMP time-exceeded replies).
    pub fn set_ttl(&mut self, ttl: u8) {
        self.ttl = ttl;
    }

    /// The current TTL.
    pub fn ttl(&self) -> u8 {
        self.ttl
    }

    /// Send `payload` as a UDP datagram from `src_port` to `dst_ip:dst_port`.
    /// Resolves the next-hop MAC (cached after the first call), builds the UDP
    /// datagram (checksummed over the IPv4 pseudo-header), frames it, and sends.
    pub async fn send_to(
        &mut self,
        dst_ip: Ipv4Addr,
        dst_port: u16,
        src_port: u16,
        payload: &[u8],
    ) -> Result<(), WireError> {
        // The next hop is the destination when it is on-link, the gateway otherwise
        // (the host-config routing decision; without a netmask this is `dst_ip`, the
        // pre-N4c behaviour).
        let hop = self.next_hop(dst_ip);
        let dst_mac =
            wire::resolve_next_hop(&mut self.cache, self.src_mac, self.src_ip, hop).await?;

        let mut datagram = [0u8; wire::MAX_FRAME - wire::L4_OFFSET];
        let dlen = build_v4(
            self.src_ip,
            dst_ip,
            src_port,
            dst_port,
            payload,
            &mut datagram,
        )
        .ok_or(WireError::TooBig)?;

        let framing = wire::Ipv4Framing {
            dst_mac,
            src_mac: self.src_mac,
            ttl: self.ttl,
            protocol: ip::proto::UDP,
            src_ip: self.src_ip,
            dst_ip,
        };
        let mut frame = [0u8; wire::MAX_FRAME];
        let flen = wire::frame_ipv4(&framing, &datagram[..dlen], &mut frame)?;
        wire::send_frame(&frame[..flen]).await
    }

    /// Poll for the next UDP-over-IPv4 datagram, copying its payload into `buf`.
    /// Skips non-IPv4 / non-UDP frames and any datagram whose checksum fails.
    /// Bounded by [`RECV_RETRIES`]; returns `WireError::ArpTimeout` if none
    /// arrives (reusing the timeout variant - the caller retransmits).
    pub async fn recv_from(&mut self, buf: &mut [u8]) -> Result<Received, WireError> {
        self.recv_inner(buf, None).await
    }

    /// Wait up to `timeout_ns` for the next UDP-over-IPv4 datagram, **parking** in the
    /// kernel instead of polling ([`wire::recv_frame_timeout`]). Returns
    /// `WireError::ArpTimeout` if the deadline elapses with nothing for us.
    ///
    /// The duration-bounded twin of [`recv_from`](Self::recv_from). Prefer it: a
    /// deadline in nanoseconds means the same thing on every ISA, whereas
    /// [`RECV_RETRIES`] is an iteration count whose wall-clock length depends on how
    /// the ISA's receive path happens to work. `recv_from` is kept unchanged for the
    /// callers that want the poll drain.
    pub async fn recv_from_timeout(
        &mut self,
        buf: &mut [u8],
        timeout_ns: u64,
    ) -> Result<Received, WireError> {
        self.recv_inner(buf, Some(timeout_ns)).await
    }

    /// The shared receive body: `None` = poll [`RECV_RETRIES`] times, `Some(ns)` =
    /// park up to a per-frame deadline, bounded by [`RECV_FRAME_BUDGET`] frames so
    /// unrelated traffic cannot stretch the wait.
    async fn recv_inner(
        &mut self,
        buf: &mut [u8],
        timeout_ns: Option<u64>,
    ) -> Result<Received, WireError> {
        let mut frame = [0u8; wire::MAX_FRAME];
        let attempts = match timeout_ns {
            None => RECV_RETRIES,
            Some(_) => RECV_FRAME_BUDGET,
        };
        for _ in 0..attempts {
            let n = match timeout_ns {
                None => wire::recv_frame(&mut frame).await?,
                Some(t) => wire::recv_frame_timeout(&mut frame, t).await?,
            };
            if n == 0 {
                if timeout_ns.is_some() {
                    break; // the deadline elapsed with nothing on the link
                }
                continue;
            }
            let Some(parsed) = wire::parse_ipv4(&frame[..n]) else {
                continue;
            };
            if parsed.header.protocol != ip::proto::UDP {
                continue;
            }
            let (start, end) = parsed.l4;
            let datagram = &frame[start..end];
            let Some(hdr) = UdpHeader::parse(datagram) else {
                continue;
            };
            if !verify_checksum_v4(parsed.header.src, parsed.header.dst, datagram) {
                continue;
            }
            let Some(payload) = hdr.payload(datagram) else {
                continue;
            };
            if payload.len() > buf.len() {
                return Err(WireError::TooBig);
            }
            buf[..payload.len()].copy_from_slice(payload);
            return Ok(Received {
                src_ip: parsed.header.src,
                src_port: hdr.src_port,
                dst_port: hdr.dst_port,
                len: payload.len(),
            });
        }
        Err(WireError::ArpTimeout)
    }
}
