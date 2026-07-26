//! IPv4 + IPv6 headers and the Internet checksum (docs/NETSTACK.md L3).
//!
//! ## The ones-complement Internet checksum (RFC 1071)
//! [`checksum16`] sums the data as big-endian 16-bit words into a 32-bit
//! accumulator, folds the carries back in (the **end-around carry**), and takes
//! the one's complement. A trailing odd byte is treated as the high byte of a
//! final word (low byte zero). This is the IPv4 header checksum and the reusable
//! core UDP/ICMP will call in N1b - UDP/TCP prepend a **pseudo-header**
//! (src/dst address + protocol + length), which is why [`Checksum`] is an
//! accumulator that can be fed several slices (address, pseudo-header, payload)
//! and correctly carries a half-word across slice boundaries.
//!
//! The path is **scalar and portable** (no `cfg(target_arch)`), which is the
//! correctness requirement. A SIMD fast path may be added later behind a runtime
//! choice with this scalar routine as its oracle (the `json` precedent), never
//! replacing it.
//!
//! ## Validation
//! A header is valid iff the checksum over the whole header (checksum field
//! included) folds to `0xFFFF`, i.e. [`checksum16`] returns `0`. So to build:
//! zero the field, compute [`checksum16`], store it. To verify: run
//! [`checksum16`] over the header as received and check it is `0`.

/// An accumulator for the ones-complement Internet checksum. Feed it bytes with
/// [`Checksum::add`] (any number of slices, any lengths), then [`Checksum::finish`].
/// It carries a leftover odd byte across `add` calls so a pseudo-header + payload
/// summed as separate slices gives the same result as one contiguous buffer.
#[derive(Clone)]
pub struct Checksum {
    sum: u32,
    /// A high byte from an odd-length previous chunk, awaiting its low byte.
    pending: Option<u8>,
}

impl Default for Checksum {
    fn default() -> Self {
        Checksum::new()
    }
}

impl Checksum {
    /// A fresh, zeroed accumulator.
    pub const fn new() -> Checksum {
        Checksum {
            sum: 0,
            pending: None,
        }
    }

    /// Fold `data` into the running sum as big-endian 16-bit words.
    pub fn add(&mut self, data: &[u8]) {
        let mut i = 0;
        // Pair a pending high byte with the first byte of this chunk.
        if let Some(hi) = self.pending.take() {
            if data.is_empty() {
                self.pending = Some(hi);
                return;
            }
            self.sum += u16::from_be_bytes([hi, data[0]]) as u32;
            i = 1;
        }
        // Whole 16-bit words.
        while i + 1 < data.len() {
            self.sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
            i += 2;
        }
        // A trailing odd byte becomes the high byte of the next word.
        if i < data.len() {
            self.pending = Some(data[i]);
        }
    }

    /// Fold carries and take the one's complement, yielding the checksum field.
    pub fn finish(mut self) -> u16 {
        if let Some(hi) = self.pending.take() {
            // A dangling high byte pads with a zero low byte.
            self.sum += (hi as u32) << 8;
        }
        let mut s = self.sum;
        while (s >> 16) != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        !(s as u16)
    }
}

/// The Internet checksum of `data` (RFC 1071): the value to store in a zeroed
/// checksum field. Run over a header whose checksum field is already set, a
/// correct header yields `0`.
pub fn checksum16(data: &[u8]) -> u16 {
    let mut c = Checksum::new();
    c.add(data);
    c.finish()
}

/// An IPv4 address (4 octets, network order as written).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr([a, b, c, d])
    }
    pub const fn octets(&self) -> [u8; 4] {
        self.0
    }
}

/// An IPv6 address (16 octets).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv6Addr(pub [u8; 16]);

impl Ipv6Addr {
    pub const fn octets(&self) -> [u8; 16] {
        self.0
    }
}

/// Either an IPv4 or an IPv6 address.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// The fixed IPv4 header length (no options) in bytes.
pub const IPV4_HEADER_LEN: usize = 20;
/// The fixed IPv6 header length in bytes.
pub const IPV6_HEADER_LEN: usize = 40;

/// The default IPv4 **TTL** (RFC 1122 §3.2.1.7 recommends 64) for a
/// locally-originated datagram. The IPv4 TTL and the IPv6 hop limit are the same
/// concept - the number of forwarding hops a datagram may still cross, one byte
/// each - so rheo-net treats them symmetrically (this constant is the default for
/// both; the endpoints stamp it and traceroute overrides it per probe).
pub const DEFAULT_TTL: u8 = 64;
/// The default IPv6 **hop limit** - the v6 equivalent of [`DEFAULT_TTL`], same
/// value and same meaning (RFC 8200 §3, the "Hop Limit" field).
pub const DEFAULT_HOP_LIMIT: u8 = DEFAULT_TTL;

/// The **forwarding-plane IPv4 TTL decrement** - the primitive a rheo-net node
/// acting as a router or firewall runs on the forward path (docs/NETSTACK.md).
/// `hdr` is an on-wire IPv4 header (the checksum field included, as received).
///
/// Because a datagram whose TTL reaches 0 must never be forwarded (RFC 791), this
/// returns `None` when the TTL is already `0` or `1` - the datagram **expires on
/// this hop**, so the caller drops it and emits an ICMP Time Exceeded
/// ([`crate::icmp::build_time_exceeded`]). On a real forward it decrements the TTL
/// and **recomputes the header checksum** (a full recompute via [`checksum16`],
/// the scalar oracle - so the forwarded header stays valid) and returns `Some`.
pub fn decrement_ttl(hdr: &mut [u8]) -> Option<()> {
    if hdr.len() < IPV4_HEADER_LEN {
        return None;
    }
    // TTL 0 or 1 expires here: forwarding a TTL-1 datagram would make it 0.
    if hdr[8] <= 1 {
        return None;
    }
    hdr[8] -= 1;
    // Zero the checksum field, recompute over the header, store it back.
    hdr[10] = 0;
    hdr[11] = 0;
    let ck = checksum16(&hdr[..IPV4_HEADER_LEN]);
    hdr[10..12].copy_from_slice(&ck.to_be_bytes());
    Some(())
}

/// The **forwarding-plane IPv6 hop-limit decrement** - the v6 equivalent of
/// [`decrement_ttl`]. IPv6 has no header checksum (RFC 8200), so this only
/// decrements the hop-limit byte. Returns `None` when the hop limit is already
/// `0` or `1` (the packet expires - the caller drops it and emits an ICMPv6 Time
/// Exceeded, [`crate::icmp::build_time_exceeded_v6`]).
pub fn decrement_hop_limit(hdr: &mut [u8]) -> Option<()> {
    if hdr.len() < IPV6_HEADER_LEN {
        return None;
    }
    if hdr[7] <= 1 {
        return None;
    }
    hdr[7] -= 1;
    Some(())
}

/// IP protocol numbers N1a names (UDP/ICMP land in N1b).
pub mod proto {
    pub const ICMP: u8 = 1;
    pub const TCP: u8 = 6;
    pub const UDP: u8 = 17;
    pub const ICMPV6: u8 = 58;
}

/// A fixed-length (no-options) IPv4 header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv4Header {
    /// Differentiated services / ECN (the old TOS byte).
    pub dscp_ecn: u8,
    /// Total length: header + payload.
    pub total_len: u16,
    pub identification: u16,
    /// Flags (3 bits) + fragment offset (13 bits), host order.
    pub flags_frag: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
}

impl Ipv4Header {
    /// Write the 20-byte header into `out[..20]` with a correct checksum.
    /// Version/IHL are fixed at 4/5 (no options). Returns the length written, or
    /// `None` if `out` is too small.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < IPV4_HEADER_LEN {
            return None;
        }
        out[0] = 0x45; // version 4, IHL 5 (20 bytes)
        out[1] = self.dscp_ecn;
        out[2..4].copy_from_slice(&self.total_len.to_be_bytes());
        out[4..6].copy_from_slice(&self.identification.to_be_bytes());
        out[6..8].copy_from_slice(&self.flags_frag.to_be_bytes());
        out[8] = self.ttl;
        out[9] = self.protocol;
        out[10..12].copy_from_slice(&[0, 0]); // checksum zero during computation
        out[12..16].copy_from_slice(&self.src.0);
        out[16..20].copy_from_slice(&self.dst.0);
        let ck = checksum16(&out[0..IPV4_HEADER_LEN]);
        out[10..12].copy_from_slice(&ck.to_be_bytes());
        Some(IPV4_HEADER_LEN)
    }

    /// Parse a 20-byte fixed IPv4 header from `buf`. Returns `None` if `buf` is
    /// too short, the version is not 4, or the IHL is not 5 (options unsupported
    /// in N1a).
    pub fn parse(buf: &[u8]) -> Option<Ipv4Header> {
        if buf.len() < IPV4_HEADER_LEN {
            return None;
        }
        if buf[0] != 0x45 {
            return None;
        }
        Some(Ipv4Header {
            dscp_ecn: buf[1],
            total_len: u16::from_be_bytes([buf[2], buf[3]]),
            identification: u16::from_be_bytes([buf[4], buf[5]]),
            flags_frag: u16::from_be_bytes([buf[6], buf[7]]),
            ttl: buf[8],
            protocol: buf[9],
            src: Ipv4Addr([buf[12], buf[13], buf[14], buf[15]]),
            dst: Ipv4Addr([buf[16], buf[17], buf[18], buf[19]]),
        })
    }

    /// True if the 20-byte header in `buf` (checksum field included) is valid.
    pub fn verify_checksum(buf: &[u8]) -> bool {
        buf.len() >= IPV4_HEADER_LEN && checksum16(&buf[0..IPV4_HEADER_LEN]) == 0
    }
}

/// A fixed IPv6 header (40 bytes; no checksum - IPv6 has none).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ipv6Header {
    /// Traffic class (8 bits).
    pub traffic_class: u8,
    /// Flow label (20 bits, low bits used).
    pub flow_label: u32,
    /// Payload length after this header.
    pub payload_len: u16,
    /// Next header (protocol).
    pub next_header: u8,
    pub hop_limit: u8,
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
}

impl Ipv6Header {
    /// Write the 40-byte header into `out[..40]`. Version is fixed at 6.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < IPV6_HEADER_LEN {
            return None;
        }
        // version (4b) | traffic class (8b) | flow label (20b)
        let vtf: u32 =
            (6u32 << 28) | ((self.traffic_class as u32) << 20) | (self.flow_label & 0x000F_FFFF);
        out[0..4].copy_from_slice(&vtf.to_be_bytes());
        out[4..6].copy_from_slice(&self.payload_len.to_be_bytes());
        out[6] = self.next_header;
        out[7] = self.hop_limit;
        out[8..24].copy_from_slice(&self.src.0);
        out[24..40].copy_from_slice(&self.dst.0);
        Some(IPV6_HEADER_LEN)
    }

    /// Parse a 40-byte IPv6 header. Returns `None` if too short or not version 6.
    pub fn parse(buf: &[u8]) -> Option<Ipv6Header> {
        if buf.len() < IPV6_HEADER_LEN {
            return None;
        }
        let vtf = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if (vtf >> 28) != 6 {
            return None;
        }
        let mut src = [0u8; 16];
        let mut dst = [0u8; 16];
        src.copy_from_slice(&buf[8..24]);
        dst.copy_from_slice(&buf[24..40]);
        Some(Ipv6Header {
            traffic_class: ((vtf >> 20) & 0xFF) as u8,
            flow_label: vtf & 0x000F_FFFF,
            payload_len: u16::from_be_bytes([buf[4], buf[5]]),
            next_header: buf[6],
            hop_limit: buf[7],
            src: Ipv6Addr(src),
            dst: Ipv6Addr(dst),
        })
    }
}
