//! ICMPv4 echo (docs/NETSTACK.md L3): the `ping` request/reply pair. An ICMP
//! message is `[type 1][code 1][checksum 2][rest 4][payload...]`; for echo the
//! `rest` field is `[identifier 2][sequence 2]`. The checksum is the ones-
//! complement Internet checksum over the **whole ICMP message** (no pseudo-header
//! - unlike UDP/TCP), so it reuses the N1a [`checksum16`] directly.
//!
//! ## TTL / Time Exceeded / traceroute (N1e)
//! Traceroute works by sending probes with an increasing IPv4 TTL and reading the
//! ICMP **Time Exceeded** (type 11, code 0) each router returns when the TTL hits
//! zero. N1b shipped only the settable-TTL seam ([`IcmpEndpoint::set_ttl`]); N1e
//! makes it whole: [`build_time_exceeded`]/[`parse_error`] are the Time Exceeded
//! codec (with the standard payload - the offending IP header + first 8 bytes, so
//! a probe can be correlated), [`IcmpEndpoint::recv_trace`] receives and
//! classifies a router's Time Exceeded *or* the destination's Echo Reply, and
//! [`crate::trace`] is the TTL-increment state machine that drives it.
//!
//! ## ICMPv6
//! ICMPv6 Time Exceeded is **type 3, code 0**, and its checksum is mandatory over
//! an IPv6 pseudo-header (the same [`crate::ip::Checksum`] accumulator the UDP v6
//! path uses). N1e implements the v6 codec ([`build_time_exceeded_v6`]/
//! [`verify_checksum_v6`]) and unit-proves it against a known-good oracle; the
//! *live* v6 traceroute is deferred (SLIRP's deterministic proof is IPv4 to the
//! gateway - it cannot generate intermediate ICMPv6 errors). See docs/NETSTACK.md.

use crate::arp::ArpCache;
use crate::eth::Mac;
use crate::ip::{self, Checksum, Ipv4Addr, Ipv6Addr, checksum16};
use crate::trace;
use crate::wire::{self, WireError};

/// The fixed ICMP header length (type/code/checksum/rest = 8 bytes).
pub const HEADER_LEN: usize = 8;

/// ICMP message types N1b touches.
pub const ECHO_REPLY: u8 = 0;
pub const ECHO_REQUEST: u8 = 8;
/// Time Exceeded (the traceroute reply): an intermediate router returns this
/// when it decrements the TTL to zero (ICMPv4 type 11, code 0 = TTL expired).
pub const TIME_EXCEEDED: u8 = 11;

/// ICMPv6 message types N1e touches (RFC 4443). Time Exceeded is **type 3** on
/// v6 (v4 uses 11); echo is 128/129.
pub const ICMPV6_TIME_EXCEEDED: u8 = 3;
pub const ICMPV6_ECHO_REQUEST: u8 = 128;
pub const ICMPV6_ECHO_REPLY: u8 = 129;

/// The length of the ICMP **error** header that precedes the embedded original
/// datagram: `[type 1][code 1][checksum 2][unused 4]` (RFC 792 / RFC 4443). Same
/// 8-byte shape as the echo header, but the last 4 bytes are unused, not id/seq.
pub const ERROR_HEADER_LEN: usize = 8;

/// A parsed ICMP **error** message (Time Exceeded / Destination Unreachable): the
/// type/code and the **embedded original datagram** - the offending IP header
/// plus at least its first 8 bytes, which is what lets a traceroute correlate the
/// reply to the probe it sent (for an ICMP-echo probe those 8 bytes are the echo
/// header, carrying the sequence number = the hop).
pub struct IcmpError<'a> {
    pub msg_type: u8,
    pub code: u8,
    /// The bytes after the 8-byte error header: the original IP datagram (header
    /// + >= 8 bytes) that triggered this error.
    pub original: &'a [u8],
}

/// Build an ICMPv4 **Time Exceeded** (type 11, code 0) into `out`: the 8-byte
/// error header (unused field zeroed) then `original` - the offending IP header +
/// at least its first 8 bytes (RFC 792). The checksum is over the whole message
/// (no pseudo-header, like echo). Returns the total length, or `None` if `out` is
/// too small.
pub fn build_time_exceeded(original: &[u8], out: &mut [u8]) -> Option<usize> {
    let total = ERROR_HEADER_LEN + original.len();
    if out.len() < total {
        return None;
    }
    out[0] = TIME_EXCEEDED;
    out[1] = 0; // code 0 = TTL expired in transit
    out[2..4].copy_from_slice(&[0, 0]); // checksum zero during computation
    out[4..8].copy_from_slice(&[0, 0, 0, 0]); // unused
    out[ERROR_HEADER_LEN..total].copy_from_slice(original);
    let ck = checksum16(&out[..total]);
    out[2..4].copy_from_slice(&ck.to_be_bytes());
    Some(total)
}

/// Parse the front of an ICMP error message (type/code + the embedded original
/// datagram). Returns `None` if `buf` is shorter than the 8-byte error header.
/// Does not verify the checksum - the caller does that ([`verify_checksum`] for
/// v4, [`verify_checksum_v6`] for v6).
pub fn parse_error(buf: &[u8]) -> Option<IcmpError<'_>> {
    if buf.len() < ERROR_HEADER_LEN {
        return None;
    }
    Some(IcmpError {
        msg_type: buf[0],
        code: buf[1],
        original: &buf[ERROR_HEADER_LEN..],
    })
}

/// The ICMPv6 checksum over the **IPv6 pseudo-header + message** (RFC 4443 §2.3).
/// Unlike ICMPv4, ICMPv6 mandates a checksum keyed to the src/dst addresses - the
/// same `[src][dst][len(4)][zero(3)][next_header]` pseudo-header UDP-over-v6 uses,
/// with next_header = 58 (ICMPv6). Reuses the N1a [`Checksum`] accumulator.
pub fn checksum_v6(src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) -> u16 {
    let len = msg.len() as u32;
    let mut c = Checksum::new();
    c.add(&src.0);
    c.add(&dst.0);
    c.add(&len.to_be_bytes());
    c.add(&[0, 0, 0, ip::proto::ICMPV6]);
    c.add(msg);
    c.finish()
}

/// Build an ICMPv6 **Time Exceeded** (type 3, code 0) into `out` with the IPv6
/// pseudo-header checksum. `original` is the offending IPv6 packet (header + as
/// much as fits). Returns the total length, or `None` if `out` is too small.
pub fn build_time_exceeded_v6(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    original: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = ERROR_HEADER_LEN + original.len();
    if out.len() < total {
        return None;
    }
    out[0] = ICMPV6_TIME_EXCEEDED;
    out[1] = 0; // code 0 = hop limit exceeded in transit
    out[2..4].copy_from_slice(&[0, 0]);
    out[4..8].copy_from_slice(&[0, 0, 0, 0]); // unused
    out[ERROR_HEADER_LEN..total].copy_from_slice(original);
    let ck = checksum_v6(src, dst, &out[..total]);
    out[2..4].copy_from_slice(&ck.to_be_bytes());
    Some(total)
}

/// True if the ICMPv6 message in `msg` verifies against the `src`/`dst`
/// pseudo-header (folds to zero).
pub fn verify_checksum_v6(src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) -> bool {
    msg.len() >= ERROR_HEADER_LEN && checksum_v6(src, dst, msg) == 0
}

/// A parsed ICMP echo header (type/code + the identifier/sequence in `rest`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EchoHeader {
    pub msg_type: u8,
    pub code: u8,
    pub ident: u16,
    pub seq: u16,
}

/// Build an ICMP **echo request** (type 8) into `out`: the 8-byte header with
/// `ident`/`seq`, then `payload`, with a correct checksum over the whole message.
/// Returns the total length, or `None` if `out` is too small.
pub fn build_echo_request(ident: u16, seq: u16, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    build_echo(ECHO_REQUEST, ident, seq, payload, out)
}

/// Build an ICMP echo message of `msg_type` (request 8 / reply 0).
pub fn build_echo(
    msg_type: u8,
    ident: u16,
    seq: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return None;
    }
    out[0] = msg_type;
    out[1] = 0; // code
    out[2..4].copy_from_slice(&[0, 0]); // checksum zero during computation
    out[4..6].copy_from_slice(&ident.to_be_bytes());
    out[6..8].copy_from_slice(&seq.to_be_bytes());
    out[HEADER_LEN..total].copy_from_slice(payload);
    let ck = checksum16(&out[..total]);
    out[2..4].copy_from_slice(&ck.to_be_bytes());
    Some(total)
}

/// Parse an ICMP echo header from the front of `buf`. Returns `None` if too short.
pub fn parse_echo(buf: &[u8]) -> Option<EchoHeader> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    Some(EchoHeader {
        msg_type: buf[0],
        code: buf[1],
        ident: u16::from_be_bytes([buf[4], buf[5]]),
        seq: u16::from_be_bytes([buf[6], buf[7]]),
    })
}

/// True if the ICMP message in `buf` (checksum field included) is intact. ICMP
/// has no pseudo-header, so a correct message folds to `0`.
pub fn verify_checksum(buf: &[u8]) -> bool {
    buf.len() >= HEADER_LEN && checksum16(buf) == 0
}

/// The RX poll budget for [`IcmpEndpoint::recv_reply`] (see `udp::RECV_RETRIES`).
pub const RECV_RETRIES: u32 = 200_000;

/// An async ICMPv4 endpoint (docs/NETSTACK.md L3). Owns the local MAC + IPv4
/// address, an [`ArpCache`], and the IPv4 TTL to stamp (the traceroute hook).
pub struct IcmpEndpoint {
    src_mac: Mac,
    src_ip: Ipv4Addr,
    ttl: u8,
    cache: ArpCache,
}

/// A received echo reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub src_ip: Ipv4Addr,
    pub ident: u16,
    pub seq: u16,
}

impl IcmpEndpoint {
    /// A new endpoint for `src_ip` reachable at `src_mac`, TTL [`wire::DEFAULT_TTL`].
    pub fn new(src_mac: Mac, src_ip: Ipv4Addr) -> IcmpEndpoint {
        IcmpEndpoint {
            src_mac,
            src_ip,
            ttl: wire::DEFAULT_TTL,
            cache: ArpCache::new(),
        }
    }

    /// Set the IPv4 TTL (the traceroute hook - a later phase increments it and
    /// reads the ICMP time-exceeded replies).
    pub fn set_ttl(&mut self, ttl: u8) {
        self.ttl = ttl;
    }

    /// The current TTL.
    pub fn ttl(&self) -> u8 {
        self.ttl
    }

    /// Send an ICMP echo request to `dst_ip` with `ident`/`seq` and `payload`.
    pub async fn send_echo(
        &mut self,
        dst_ip: Ipv4Addr,
        ident: u16,
        seq: u16,
        payload: &[u8],
    ) -> Result<(), WireError> {
        let dst_mac =
            wire::resolve_next_hop(&mut self.cache, self.src_mac, self.src_ip, dst_ip).await?;

        let mut msg = [0u8; wire::MAX_FRAME - wire::L4_OFFSET];
        let mlen = build_echo_request(ident, seq, payload, &mut msg).ok_or(WireError::TooBig)?;

        let framing = wire::Ipv4Framing {
            dst_mac,
            src_mac: self.src_mac,
            ttl: self.ttl,
            protocol: ip::proto::ICMP,
            src_ip: self.src_ip,
            dst_ip,
        };
        let mut frame = [0u8; wire::MAX_FRAME];
        let flen = wire::frame_ipv4(&framing, &msg[..mlen], &mut frame)?;
        wire::send_frame(&frame[..flen]).await
    }

    /// Poll for an ICMP **echo reply** matching `ident`/`seq`. Skips non-ICMP
    /// frames, non-echo-reply messages, and any with a bad checksum. Bounded by
    /// [`RECV_RETRIES`]; `WireError::ArpTimeout` if none arrives.
    pub async fn recv_reply(&mut self, ident: u16, seq: u16) -> Result<Reply, WireError> {
        let mut frame = [0u8; wire::MAX_FRAME];
        for _ in 0..RECV_RETRIES {
            let n = wire::recv_frame(&mut frame).await?;
            if n == 0 {
                continue;
            }
            let Some(parsed) = wire::parse_ipv4(&frame[..n]) else {
                continue;
            };
            if parsed.header.protocol != ip::proto::ICMP {
                continue;
            }
            let (start, end) = parsed.l4;
            let msg = &frame[start..end];
            if !verify_checksum(msg) {
                continue;
            }
            let Some(echo) = parse_echo(msg) else {
                continue;
            };
            if echo.msg_type == ECHO_REPLY && echo.ident == ident && echo.seq == seq {
                return Ok(Reply {
                    src_ip: parsed.header.src,
                    ident: echo.ident,
                    seq: echo.seq,
                });
            }
        }
        Err(WireError::ArpTimeout)
    }

    /// Receive the next ICMP message addressed to us and classify it as a
    /// **traceroute response** matching `ident`: an Echo Reply from the
    /// destination, or a Time Exceeded from an intermediate router (correlated by
    /// the echo sequence embedded in the router's original-datagram copy). Skips
    /// non-ICMP frames, bad checksums, and unrelated messages. Bounded by
    /// [`RECV_RETRIES`]; `WireError::ArpTimeout` if none arrives. This is the live
    /// receive side of [`crate::trace::Tracer::run`].
    pub async fn recv_trace(&mut self, ident: u16) -> Result<trace::Response, WireError> {
        let mut frame = [0u8; wire::MAX_FRAME];
        for _ in 0..RECV_RETRIES {
            let n = wire::recv_frame(&mut frame).await?;
            if n == 0 {
                continue;
            }
            let Some(parsed) = wire::parse_ipv4(&frame[..n]) else {
                continue;
            };
            if parsed.header.protocol != ip::proto::ICMP {
                continue;
            }
            let (start, end) = parsed.l4;
            let msg = &frame[start..end];
            if !verify_checksum(msg) {
                continue;
            }
            if let Some(r) = trace::classify(msg, parsed.header.src, ident) {
                return Ok(r);
            }
        }
        Err(WireError::ArpTimeout)
    }

    /// Send an echo request and wait for its reply, retransmitting up to
    /// `attempts` times (a momentary RX miss should not fail the proof). Returns
    /// the reply, or the last error.
    pub async fn ping(
        &mut self,
        dst_ip: Ipv4Addr,
        ident: u16,
        seq: u16,
        payload: &[u8],
        attempts: u32,
    ) -> Result<Reply, WireError> {
        let mut last = WireError::ArpTimeout;
        for _ in 0..attempts.max(1) {
            if let Err(e) = self.send_echo(dst_ip, ident, seq, payload).await {
                last = e;
                continue;
            }
            match self.recv_reply(ident, seq).await {
                Ok(r) => return Ok(r),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}
