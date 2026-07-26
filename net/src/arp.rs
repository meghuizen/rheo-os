//! ARP: request/reply parse+build, a cache, and an async resolver
//! (docs/NETSTACK.md L2). ARP maps an IPv4 address to a MAC on the local link.
//! An ARP packet (28 bytes for Ethernet/IPv4) rides inside an Ethernet frame with
//! ethertype `0x0806`:
//!
//! `[htype 2][ptype 2][hlen 1][plen 1][oper 2][sha 6][spa 4][tha 6][tpa 4]`
//!
//! all big-endian. [`resolve`] sends a broadcast request over `librheo::net` and
//! waits (bounded retries) for the reply, populating an [`ArpCache`].

use alloc::collections::BTreeMap;

use librheo::net;
use librheo::time::Instant;

use crate::eth::{self, Mac};
use crate::ip::Ipv4Addr;

/// The ARP payload length for Ethernet/IPv4.
pub const PACKET_LEN: usize = 28;
/// A full broadcast ARP request frame (14-byte Ethernet header + 28-byte ARP).
pub const REQUEST_FRAME_LEN: usize = eth::HEADER_LEN + PACKET_LEN;

/// Hardware type: Ethernet.
pub const HTYPE_ETHERNET: u16 = 1;
/// Protocol type: IPv4.
pub const PTYPE_IPV4: u16 = 0x0800;
/// Operation: request.
pub const OP_REQUEST: u16 = 1;
/// Operation: reply.
pub const OP_REPLY: u16 = 2;

/// A parsed/buildable ARP packet (Ethernet/IPv4 shapes only).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub oper: u16,
    /// Sender hardware (MAC) address.
    pub sha: Mac,
    /// Sender protocol (IPv4) address.
    pub spa: Ipv4Addr,
    /// Target hardware (MAC) address.
    pub tha: Mac,
    /// Target protocol (IPv4) address.
    pub tpa: Ipv4Addr,
}

impl ArpPacket {
    /// Write the 28-byte ARP payload into `out[..28]` (htype/ptype/hlen/plen are
    /// fixed for Ethernet/IPv4). Returns the length written, or `None` if `out`
    /// is too small.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < PACKET_LEN {
            return None;
        }
        out[0..2].copy_from_slice(&HTYPE_ETHERNET.to_be_bytes());
        out[2..4].copy_from_slice(&PTYPE_IPV4.to_be_bytes());
        out[4] = 6; // hlen
        out[5] = 4; // plen
        out[6..8].copy_from_slice(&self.oper.to_be_bytes());
        out[8..14].copy_from_slice(&self.sha.0);
        out[14..18].copy_from_slice(&self.spa.0);
        out[18..24].copy_from_slice(&self.tha.0);
        out[24..28].copy_from_slice(&self.tpa.0);
        Some(PACKET_LEN)
    }

    /// Parse a 28-byte Ethernet/IPv4 ARP payload. Returns `None` if too short or
    /// the htype/ptype/hlen/plen are not Ethernet/IPv4.
    pub fn parse(buf: &[u8]) -> Option<ArpPacket> {
        if buf.len() < PACKET_LEN {
            return None;
        }
        let htype = u16::from_be_bytes([buf[0], buf[1]]);
        let ptype = u16::from_be_bytes([buf[2], buf[3]]);
        if htype != HTYPE_ETHERNET || ptype != PTYPE_IPV4 || buf[4] != 6 || buf[5] != 4 {
            return None;
        }
        let mut sha = [0u8; 6];
        let mut tha = [0u8; 6];
        sha.copy_from_slice(&buf[8..14]);
        tha.copy_from_slice(&buf[18..24]);
        Some(ArpPacket {
            oper: u16::from_be_bytes([buf[6], buf[7]]),
            sha: Mac(sha),
            spa: Ipv4Addr([buf[14], buf[15], buf[16], buf[17]]),
            tha: Mac(tha),
            tpa: Ipv4Addr([buf[24], buf[25], buf[26], buf[27]]),
        })
    }
}

/// Build a complete broadcast ARP **request** frame ("who has `target_ip`, tell
/// `src_ip`") into a fixed-size buffer. Built through [`eth`] + [`ArpPacket`],
/// not hand-laid bytes.
pub fn build_request(
    src_mac: Mac,
    src_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) -> [u8; REQUEST_FRAME_LEN] {
    let mut frame = [0u8; REQUEST_FRAME_LEN];
    let header = eth::Header {
        dst: eth::BROADCAST,
        src: src_mac,
        ethertype: eth::ethertype::ARP,
    };
    // The Ethernet header, then the ARP payload after it.
    header.write(&mut frame[..eth::HEADER_LEN]).unwrap();
    let arp = ArpPacket {
        oper: OP_REQUEST,
        sha: src_mac,
        spa: src_ip,
        tha: Mac([0; 6]), // unknown target hardware address
        tpa: target_ip,
    };
    arp.write(&mut frame[eth::HEADER_LEN..]).unwrap();
    frame
}

/// One cache entry: the resolved MAC and the tick it was learned at.
#[derive(Copy, Clone)]
struct Entry {
    mac: Mac,
    learned: Instant,
}

/// A small ARP cache (IPv4 -> MAC). Backed by a `BTreeMap` keyed on the 4-octet
/// address. TTL/eviction are minimal for N1a: [`ArpCache::lookup`] optionally
/// treats an entry older than a caller-supplied age as stale. A full aging /
/// probe / eviction policy is an N1b refinement.
#[derive(Default)]
pub struct ArpCache {
    map: BTreeMap<[u8; 4], Entry>,
}

impl ArpCache {
    /// An empty cache.
    pub fn new() -> ArpCache {
        ArpCache {
            map: BTreeMap::new(),
        }
    }

    /// Insert or refresh `ip -> mac`, stamping it with the current tick.
    pub fn insert(&mut self, ip: Ipv4Addr, mac: Mac) {
        self.map.insert(
            ip.0,
            Entry {
                mac,
                learned: Instant::now(),
            },
        );
    }

    /// Look up `ip`, returning its MAC if present (no aging).
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<Mac> {
        self.map.get(&ip.0).map(|e| e.mac)
    }

    /// Look up `ip`, treating an entry older than `max_age_ticks` as absent. A
    /// minimal TTL: the caller supplies the age budget (per-ISA ticks), so this
    /// stays portable. `max_age_ticks == 0` means "never expire".
    pub fn lookup_fresh(&self, ip: Ipv4Addr, max_age_ticks: u64) -> Option<Mac> {
        let e = self.map.get(&ip.0)?;
        if max_age_ticks != 0 && e.learned.elapsed_ticks() > max_age_ticks {
            return None;
        }
        Some(e.mac)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True if the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The retry budget for [`resolve`]'s RX poll. Each `net::try_recv` is a doorbell (a
/// VM exit that lets QEMU's SLIRP backend run), so a reply lands within a handful
/// of iterations; the cap only guards a wholly non-delivering backend. Bounded
/// retries keep the proof deterministic under QEMU; a wall-clock timeout via
/// `librheo::time::timeout` is available for a real deployment.
pub const RESOLVE_RETRIES: u32 = 200_000;

/// The error [`resolve`] returns.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The raw-frame TX/RX path failed.
    Net,
    /// No matching ARP reply within the retry budget.
    Timeout,
}

/// Resolve `target_ip` to a MAC on the local link (docs/NETSTACK.md L2). Returns
/// a cached entry immediately; otherwise broadcasts an ARP request through
/// [`build_request`] + `librheo::net::send`, polls `net::try_recv` for the reply
/// (parsing via [`eth::Frame`] + [`ArpPacket::parse`]), inserts it into `cache`,
/// and returns the MAC. Bounded by [`RESOLVE_RETRIES`].
pub async fn resolve(
    cache: &mut ArpCache,
    src_mac: Mac,
    src_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) -> Result<Mac, ResolveError> {
    if let Some(mac) = cache.lookup(target_ip) {
        return Ok(mac);
    }

    let req = build_request(src_mac, src_ip, target_ip);
    net::send(&req).await.map_err(|_| ResolveError::Net)?;

    let mut buf = [0u8; 1600];
    for _ in 0..RESOLVE_RETRIES {
        let n = net::try_recv(&mut buf)
            .await
            .map_err(|_| ResolveError::Net)?;
        if n == 0 {
            continue; // nothing yet - poll again
        }
        let Some(frame) = eth::Frame::parse(&buf[..n]) else {
            continue;
        };
        if frame.ethertype() != eth::ethertype::ARP {
            continue;
        }
        let Some(arp) = ArpPacket::parse(frame.payload()) else {
            continue;
        };
        if arp.oper == OP_REPLY && arp.spa == target_ip {
            cache.insert(arp.spa, arp.sha);
            return Ok(arp.sha);
        }
    }
    Err(ResolveError::Timeout)
}
