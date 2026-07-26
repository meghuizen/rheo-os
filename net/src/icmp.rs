//! ICMPv4 echo (docs/NETSTACK.md L3): the `ping` request/reply pair. An ICMP
//! message is `[type 1][code 1][checksum 2][rest 4][payload...]`; for echo the
//! `rest` field is `[identifier 2][sequence 2]`. The checksum is the ones-
//! complement Internet checksum over the **whole ICMP message** (no pseudo-header
//! - unlike UDP/TCP), so it reuses the N1a [`checksum16`] directly.
//!
//! ## The TTL hook for traceroute
//! Traceroute works by sending probes with an increasing IPv4 TTL and reading the
//! ICMP **time-exceeded** (type 11) each router returns when the TTL hits zero.
//! N1b exposes the seam: the IPv4 TTL is settable per send ([`IcmpEndpoint::set_ttl`],
//! mirroring [`crate::udp::UdpEndpoint::set_ttl`] and [`crate::wire::frame_ipv4`]).
//! The full TTL-increment loop + time-exceeded parsing is deferred to N7
//! (docs/NETSTACK.md), so N1b proves only the echo request/reply.
//!
//! ## ICMPv6
//! ICMPv6 echo (type 128/129) is deferred: its checksum is mandatory over an IPv6
//! pseudo-header (the same accumulator the UDP v6 path uses), but the deterministic
//! SLIRP proof here is ICMPv4 to the gateway. See docs/NETSTACK.md N1b/N7.

use crate::arp::ArpCache;
use crate::eth::Mac;
use crate::ip::{self, Ipv4Addr, checksum16};
use crate::wire::{self, WireError};

/// The fixed ICMP header length (type/code/checksum/rest = 8 bytes).
pub const HEADER_LEN: usize = 8;

/// ICMP message types N1b touches.
pub const ECHO_REPLY: u8 = 0;
pub const ECHO_REQUEST: u8 = 8;
/// Time exceeded (the traceroute reply) - parsed in a later phase.
pub const TIME_EXCEEDED: u8 = 11;

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
