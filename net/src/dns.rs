//! A caching DNS client (docs/NETSTACK.md N1c): message codec, an async resolver
//! over [`crate::udp`], an LRU + TTL cache, a blocklist, and configurable
//! resolvers + a static hosts table.
//!
//! ## The message format
//! A DNS message is a 12-byte header, then `qdcount` questions, then answer /
//! authority / additional resource records. The header is
//! `[id 2][flags 2][qdcount 2][ancount 2][nscount 2][arcount 2]` (big-endian). A
//! name is a sequence of length-prefixed labels ending in a zero length byte; a
//! record is `[name][type 2][class 2][ttl 4][rdlength 2][rdata...]`.
//!
//! ## Name compression (the correctness- and security-critical piece)
//! A label's length byte carries its kind in the top two bits:
//! - `0b00`: a normal label, the low 6 bits are its length (0-63).
//! - `0b11`: a **compression pointer** - the low 6 bits plus the next byte form a
//!   14-bit offset from the start of the message; parsing jumps there and
//!   continues. This is how a response reuses the question's name without
//!   repeating it.
//!
//! The classic DNS-parser bug is a crafted pointer **loop** (a pointer to itself,
//! or a cycle) that hangs the parser, or a pointer past the buffer that reads out
//! of bounds. [`read_name`] defends against both: it caps the number of pointer
//! jumps ([`MAX_JUMPS`]), rejects any offset at or past the message end, and caps
//! the assembled name length at [`MAX_NAME_LEN`] (255). A malicious packet gets a
//! clean [`DnsError::Parse`], never a hang.
//!
//! ## The cache
//! [`Cache`] is an LRU keyed on `(name, qtype)` with per-entry TTL expiry: a
//! lookup evicts an expired entry, and when the cap is reached the
//! least-recently-used entry is evicted. It works on an **opaque monotonic
//! clock** the caller supplies (ticks), so the cache math is exact and portable
//! and the deterministic proof drives it directly. The resolver scales a DNS TTL
//! (seconds) to that clock with [`Config::ticks_per_sec`] (a nominal calibration;
//! TTL-expiry precision on the live path is best-effort - the deterministic tests
//! never depend on it).
//!
//! ## The blocklist (built for large lists)
//! [`Blocklist`] holds exact names in a from-scratch open-addressing [`HashSet`]
//! (FNV-1a, O(1) average - the shape a multi-million-entry list needs) plus a
//! small list of wildcard suffixes (`*.ads.example`). A blocked name resolves to
//! `Err(Blocked)` (or a configured sinkhole) with **no network query**. For a
//! truly huge list the `HashSet` slots + interned names would live in a
//! grant-backed [`librheo::mem`] arena rather than the general heap; N1c uses the
//! heap at test scale and documents that arena path.
//!
//! ## Two postures
//! The **codec** half of this module - name reading/writing, question and
//! response parsing, the [`Cache`], the [`Blocklist`], the [`HostsTable`] - is
//! always compiled: it is pure parsing over `alloc`, with no librheo and no NIC.
//! That is what lets **mDNS** ([`crate::zeroconf::mdns`]) reuse this exact codec in
//! either posture, and it mirrors how `http1`/`http2` are posture-independent. Only
//! [`Config`] and [`Resolver`] - which need the clock and a
//! [`UdpEndpoint`](crate::udp::UdpEndpoint) - sit behind the `hosted` feature.
//!
//! ## Deferred (documented)
//! Negative caching (caching an NXDOMAIN for a short TTL) is deferred - each
//! NXDOMAIN currently re-queries; the seam is `Config` + `Cache`. AAAA is fully
//! supported in the codec and resolver; only the *live* proof is A (SLIRP proxies
//! to the host resolver). Off-link routing now reads the
//! [`crate::hostcfg`] store (rheo-net N4c); a multi-route table is still deferred.

use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "hosted")]
use librheo::time::{self, Duration, Instant};

use crate::ip::{IpAddr, Ipv4Addr, Ipv6Addr};
#[cfg(feature = "hosted")]
use crate::udp::UdpEndpoint;
#[cfg(feature = "hosted")]
use crate::wire::WireError;

// ---- record types / codes ----

/// A-record type (IPv4 address).
pub const TYPE_A: u16 = 1;
/// NS-record type.
pub const TYPE_NS: u16 = 2;
/// CNAME-record type (canonical name).
pub const TYPE_CNAME: u16 = 5;
/// AAAA-record type (IPv6 address).
pub const TYPE_AAAA: u16 = 28;
/// The Internet class.
pub const CLASS_IN: u16 = 1;
/// The NXDOMAIN response code (name does not exist).
pub const RCODE_NXDOMAIN: u8 = 3;

/// The maximum assembled name length (RFC 1035): a longer name is malformed.
pub const MAX_NAME_LEN: usize = 255;
/// The cap on pointer jumps while reading one name. A valid name has at most a
/// handful of labels; exceeding this means a crafted loop, so it is rejected.
pub const MAX_JUMPS: u32 = 128;

/// The query type a resolver asks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QType {
    /// An IPv4 address (A record).
    A,
    /// An IPv6 address (AAAA record).
    Aaaa,
    /// A canonical name (CNAME record).
    Cname,
}

impl QType {
    /// The on-wire type code.
    pub fn as_u16(self) -> u16 {
        match self {
            QType::A => TYPE_A,
            QType::Aaaa => TYPE_AAAA,
            QType::Cname => TYPE_CNAME,
        }
    }
}

/// A resolver / codec error.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DnsError {
    /// The name is on the blocklist (refused without a query).
    Blocked,
    /// No answer arrived within the timeout/retry budget.
    Timeout,
    /// The raw-frame transport failed.
    Net,
    /// The name to encode was invalid (empty label, or a label over 63 bytes).
    BadName,
    /// The response could not be parsed (truncated, bad pointer, over-long name).
    Parse,
    /// The authoritative server says the name does not exist.
    NxDomain,
    /// A well-formed response held no address of the requested type.
    NoAddress,
}

// ---- name codec ----

/// Lowercase an ASCII byte (DNS names are case-insensitive; other bytes pass
/// through).
fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() { b + 32 } else { b }
}

/// Normalize a name for keys: strip a single trailing dot and lowercase ASCII.
/// `Example.COM.` and `example.com` become the same key. Public so mDNS
/// ([`crate::zeroconf::mdns`]) and the host-config store ([`crate::hostcfg`]) key
/// names exactly the way the resolver does.
pub fn normalize(name: &str) -> String {
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    let mut s = String::with_capacity(trimmed.len());
    for &b in trimmed.as_bytes() {
        s.push(ascii_lower(b) as char);
    }
    s
}

/// Read a (possibly compressed) DNS name from `msg` starting at `start`, appending
/// the lowercased dotted name to `out`. Returns the stream position **after** the
/// name (past the terminating zero, or past the first 2-byte pointer - a pointer
/// ends the name in the record stream even though parsing follows it).
///
/// Loop- and bounds-safe: pointer jumps are capped at [`MAX_JUMPS`], an offset at
/// or past the message end is rejected, and the assembled name is capped at
/// [`MAX_NAME_LEN`]. Any violation returns [`DnsError::Parse`] - never a hang.
pub fn read_name(msg: &[u8], start: usize, out: &mut String) -> Result<usize, DnsError> {
    let mut pos = start;
    let mut jumps = 0u32;
    // The position after the name in the record stream, set the first time a
    // pointer is taken (or at the terminating zero if no pointer is seen).
    let mut after: Option<usize> = None;
    let mut total = 0usize;
    loop {
        if pos >= msg.len() {
            return Err(DnsError::Parse);
        }
        let len = msg[pos];
        match len & 0xC0 {
            0x00 => {
                if len == 0 {
                    pos += 1;
                    if after.is_none() {
                        after = Some(pos);
                    }
                    break;
                }
                let l = len as usize;
                let s = pos + 1;
                let e = s + l;
                if e > msg.len() {
                    return Err(DnsError::Parse);
                }
                total += l + 1;
                if total > MAX_NAME_LEN {
                    return Err(DnsError::Parse);
                }
                if !out.is_empty() {
                    out.push('.');
                }
                for &b in &msg[s..e] {
                    out.push(ascii_lower(b) as char);
                }
                pos = e;
            }
            0xC0 => {
                if pos + 1 >= msg.len() {
                    return Err(DnsError::Parse);
                }
                let off = (((len & 0x3F) as usize) << 8) | (msg[pos + 1] as usize);
                if after.is_none() {
                    after = Some(pos + 2);
                }
                jumps += 1;
                if jumps > MAX_JUMPS {
                    return Err(DnsError::Parse);
                }
                if off >= msg.len() {
                    return Err(DnsError::Parse);
                }
                pos = off;
            }
            // 0x40 and 0x80 are reserved label kinds: reject them.
            _ => return Err(DnsError::Parse),
        }
    }
    after.ok_or(DnsError::Parse)
}

/// Write `name` as length-prefixed labels plus the terminating root label into
/// `out`, starting at `pos`. Returns the new position, or `None` if `out` is too
/// small or a label is invalid (over 63 bytes). Empty labels (a leading or
/// trailing dot) are skipped. **No compression** is emitted - a pointer needs a
/// message-wide offset table, and every name this crate writes is short.
///
/// Shared by [`build_query`] and the mDNS builders in [`crate::zeroconf::mdns`], so
/// label encoding exists in exactly one place.
pub fn write_name(name: &str, out: &mut [u8], mut pos: usize) -> Option<usize> {
    for label in name.split('.') {
        if label.is_empty() {
            continue; // skip an empty (trailing/leading) label
        }
        let bytes = label.as_bytes();
        if bytes.len() > 63 {
            return None;
        }
        if pos + 1 + bytes.len() > out.len() {
            return None;
        }
        out[pos] = bytes.len() as u8;
        pos += 1;
        out[pos..pos + bytes.len()].copy_from_slice(bytes);
        pos += bytes.len();
    }
    if pos + 1 > out.len() {
        return None;
    }
    out[pos] = 0; // root label
    Some(pos + 1)
}

/// Build a DNS query message into `out`: a 12-byte header with `flags`, one
/// question for `name`/`qtype` in class `qclass`. Returns the length written, or
/// `None` if `out` is too small or a label is invalid.
///
/// The general form [`build_query`] and mDNS both use: unicast DNS wants
/// `flags = 0x0100` (recursion desired) and `qclass = CLASS_IN`, while mDNS wants
/// `flags = 0` (no recursion, id 0) and may set the **QU** bit (`0x8000`) in
/// `qclass` to ask for a unicast reply (RFC 6762 §5.4).
pub fn build_question_message(
    id: u16,
    flags: u16,
    name: &str,
    qtype: u16,
    qclass: u16,
    out: &mut [u8],
) -> Option<usize> {
    if out.len() < 12 {
        return None;
    }
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // qdcount = 1
    out[6..12].copy_from_slice(&[0, 0, 0, 0, 0, 0]); // an/ns/ar = 0
    let mut pos = write_name(name, out, 12)?;
    if pos + 4 > out.len() {
        return None;
    }
    out[pos..pos + 2].copy_from_slice(&qtype.to_be_bytes());
    pos += 2;
    out[pos..pos + 2].copy_from_slice(&qclass.to_be_bytes());
    pos += 2;
    Some(pos)
}

/// Build a standard query for `name`/`qtype` (recursion desired) into `out`.
/// Returns the length written, or `None` if `out` is too small or a label is
/// invalid (empty or over 63 bytes).
pub fn build_query(id: u16, name: &str, qtype: QType, out: &mut [u8]) -> Option<usize> {
    build_question_message(id, 0x0100, name, qtype.as_u16(), CLASS_IN, out)
}

/// One parsed question from a DNS/mDNS message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The queried name (decompressed, lowercased).
    pub name: String,
    /// The on-wire query type.
    pub qtype: u16,
    /// The on-wire query class. In **mDNS** the top bit is the **QU** flag
    /// ("please answer by unicast", RFC 6762 §5.4), so the class itself is
    /// `qclass & 0x7FFF`.
    pub qclass: u16,
}

impl Question {
    /// True if the mDNS **QU** bit is set - the asker wants a unicast reply.
    pub fn unicast_response(&self) -> bool {
        self.qclass & 0x8000 != 0
    }

    /// The class with the mDNS QU bit masked off.
    pub fn class(&self) -> u16 {
        self.qclass & 0x7FFF
    }
}

/// Parse the question section of a DNS/mDNS message (the header's `qdcount`
/// questions after the 12-byte header). Names are decompressed with the same
/// loop-bounded [`read_name`] the answer path uses. Returns
/// [`DnsError::Parse`] on any malformed input.
pub fn parse_questions(msg: &[u8]) -> Result<Vec<Question>, DnsError> {
    if msg.len() < 12 {
        return Err(DnsError::Parse);
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let mut pos = 12;
    let mut out = Vec::new();
    for _ in 0..qd {
        let mut name = String::new();
        pos = read_name(msg, pos, &mut name)?;
        if pos + 4 > msg.len() {
            return Err(DnsError::Parse);
        }
        out.push(Question {
            name,
            qtype: u16::from_be_bytes([msg[pos], msg[pos + 1]]),
            qclass: u16::from_be_bytes([msg[pos + 2], msg[pos + 3]]),
        });
        pos += 4;
    }
    Ok(out)
}

/// The data of a parsed resource record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RData {
    /// An IPv4 address (A).
    A(Ipv4Addr),
    /// An IPv6 address (AAAA).
    Aaaa(Ipv6Addr),
    /// A canonical name (CNAME), decompressed.
    Cname(String),
    /// A type this codec does not decode.
    Other,
}

/// A parsed resource record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The owner name (decompressed, lowercased).
    pub name: String,
    /// The on-wire type code.
    pub rtype: u16,
    /// The on-wire class code **as received**. In **mDNS** the top bit is the
    /// **cache-flush** flag (RFC 6762 §10.2), so the real class is
    /// `class & 0x7FFF` - use [`Record::class`] / [`Record::cache_flush`] rather
    /// than comparing this field directly.
    pub class: u16,
    /// The record TTL in seconds. A TTL of 0 in mDNS is a **goodbye** (the record
    /// is going away, RFC 6762 §10.1) - see [`Record::is_goodbye`].
    pub ttl: u32,
    /// The decoded record data.
    pub data: RData,
}

impl Record {
    /// The class with the mDNS cache-flush bit masked off.
    pub fn class(&self) -> u16 {
        self.class & 0x7FFF
    }

    /// True if the mDNS **cache-flush** bit is set: the responder is asserting
    /// this record is authoritative and any cached records for the same
    /// name/type/class should be replaced, not merged (RFC 6762 §10.2).
    pub fn cache_flush(&self) -> bool {
        self.class & 0x8000 != 0
    }

    /// True if this is an mDNS **goodbye**: TTL 0 means the record is going away
    /// and a cache should drop it (RFC 6762 §10.1).
    pub fn is_goodbye(&self) -> bool {
        self.ttl == 0
    }
}

/// A parsed DNS response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The transaction id (echoed by the server).
    pub id: u16,
    /// The response code (0 = no error, 3 = NXDOMAIN).
    pub rcode: u8,
    /// The answer records.
    pub answers: Vec<Record>,
}

/// Parse a DNS response message: the header, the questions (skipped), and the
/// answer records (authority/additional are ignored). Names in answers and CNAME
/// rdata are decompressed. Returns [`DnsError::Parse`] on any malformed input.
pub fn parse_response(msg: &[u8]) -> Result<Response, DnsError> {
    if msg.len() < 12 {
        return Err(DnsError::Parse);
    }
    let id = u16::from_be_bytes([msg[0], msg[1]]);
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    let rcode = (flags & 0x000F) as u8;
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);

    let mut pos = 12;
    // Skip the questions: each is a name then qtype(2) + qclass(2).
    for _ in 0..qd {
        let mut scratch = String::new();
        pos = read_name(msg, pos, &mut scratch)?;
        pos = pos.checked_add(4).ok_or(DnsError::Parse)?;
        if pos > msg.len() {
            return Err(DnsError::Parse);
        }
    }

    let mut answers = Vec::new();
    for _ in 0..an {
        let mut name = String::new();
        pos = read_name(msg, pos, &mut name)?;
        if pos + 10 > msg.len() {
            return Err(DnsError::Parse);
        }
        let rtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
        let class = u16::from_be_bytes([msg[pos + 2], msg[pos + 3]]);
        let ttl = u32::from_be_bytes([msg[pos + 4], msg[pos + 5], msg[pos + 6], msg[pos + 7]]);
        let rdlen = u16::from_be_bytes([msg[pos + 8], msg[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > msg.len() {
            return Err(DnsError::Parse);
        }
        let data = match rtype {
            TYPE_A if rdlen == 4 => RData::A(Ipv4Addr([
                msg[pos],
                msg[pos + 1],
                msg[pos + 2],
                msg[pos + 3],
            ])),
            TYPE_AAAA if rdlen == 16 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(&msg[pos..pos + 16]);
                RData::Aaaa(Ipv6Addr(o))
            }
            TYPE_CNAME => {
                let mut cn = String::new();
                read_name(msg, pos, &mut cn)?;
                RData::Cname(cn)
            }
            _ => RData::Other,
        };
        pos += rdlen;
        answers.push(Record {
            name,
            rtype,
            class,
            ttl,
            data,
        });
    }
    Ok(Response { id, rcode, answers })
}

/// Collect the addresses of `qtype` from a response, with the minimum answer TTL
/// (seconds) - the conservative cache lifetime.
#[cfg(feature = "hosted")]
fn extract(resp: &Response, qtype: QType) -> (Vec<IpAddr>, u32) {
    let mut ips = Vec::new();
    let mut min_ttl = u32::MAX;
    for rec in &resp.answers {
        match (&rec.data, qtype) {
            (RData::A(a), QType::A) => {
                ips.push(IpAddr::V4(*a));
                min_ttl = min_ttl.min(rec.ttl);
            }
            (RData::Aaaa(a), QType::Aaaa) => {
                ips.push(IpAddr::V6(*a));
                min_ttl = min_ttl.min(rec.ttl);
            }
            _ => {}
        }
    }
    let ttl = if min_ttl == u32::MAX { 0 } else { min_ttl };
    (ips, ttl)
}

// ---- a from-scratch open-addressing hash set (blocklist backbone) ----

/// A minimal FNV-1a open-addressing hash set of names. Insert + membership are
/// O(1) average (linear probing), the shape a multi-million-entry blocklist
/// needs. No removal is supported (a blocklist only grows), which lets probing
/// skip tombstone handling. For a huge list the slots + names would be interned
/// in a grant-backed arena; here they live on the heap at test scale.
pub struct HashSet {
    slots: Vec<Option<String>>,
    len: usize,
}

impl Default for HashSet {
    fn default() -> Self {
        HashSet::new()
    }
}

impl HashSet {
    /// An empty set (16 slots).
    pub fn new() -> HashSet {
        HashSet::with_capacity(16)
    }

    /// An empty set with room for roughly `cap` entries before a grow.
    pub fn with_capacity(cap: usize) -> HashSet {
        let n = cap.next_power_of_two().max(16);
        let mut slots = Vec::with_capacity(n);
        slots.resize(n, None);
        HashSet { slots, len: 0 }
    }

    /// FNV-1a 64-bit hash of the name bytes.
    fn hash(s: &str) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in s.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Insert `s` (idempotent).
    pub fn insert(&mut self, s: &str) {
        if (self.len + 1) * 4 >= self.slots.len() * 3 {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut i = (Self::hash(s) as usize) & mask;
        loop {
            match &self.slots[i] {
                None => {
                    self.slots[i] = Some(String::from(s));
                    self.len += 1;
                    return;
                }
                Some(existing) if existing == s => return,
                _ => i = (i + 1) & mask,
            }
        }
    }

    /// True if `s` is present.
    pub fn contains(&self, s: &str) -> bool {
        let mask = self.slots.len() - 1;
        let mut i = (Self::hash(s) as usize) & mask;
        loop {
            match &self.slots[i] {
                None => return false,
                Some(existing) if existing == s => return true,
                _ => i = (i + 1) & mask,
            }
        }
    }

    fn grow(&mut self) {
        let new_cap = self.slots.len() * 2;
        let mut fresh = Vec::with_capacity(new_cap);
        fresh.resize(new_cap, None);
        let old = core::mem::replace(&mut self.slots, fresh);
        self.len = 0;
        for name in old.into_iter().flatten() {
            self.insert_owned(name);
        }
    }

    fn insert_owned(&mut self, s: String) {
        let mask = self.slots.len() - 1;
        let mut i = (Self::hash(&s) as usize) & mask;
        while self.slots[i].is_some() {
            i = (i + 1) & mask;
        }
        self.slots[i] = Some(s);
        self.len += 1;
    }

    /// Number of names.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A DNS blocklist: exact names plus wildcard suffixes. A blocked name resolves
/// to `Err(Blocked)` (or a sinkhole) with no network query.
pub struct Blocklist {
    exact: HashSet,
    /// Suffixes that block themselves and any subdomain (`ads.example` blocks
    /// `ads.example` and `*.ads.example`).
    suffixes: Vec<String>,
}

impl Default for Blocklist {
    fn default() -> Self {
        Blocklist::new()
    }
}

impl Blocklist {
    /// An empty blocklist.
    pub fn new() -> Blocklist {
        Blocklist {
            exact: HashSet::new(),
            suffixes: Vec::new(),
        }
    }

    /// Block an exact name.
    pub fn insert(&mut self, name: &str) {
        self.exact.insert(&normalize(name));
    }

    /// Block a wildcard pattern (`*.ads.example`, `.ads.example`, or
    /// `ads.example`): blocks the base name and every subdomain of it.
    pub fn insert_wildcard(&mut self, pattern: &str) {
        let base = pattern
            .strip_prefix("*.")
            .or_else(|| pattern.strip_prefix('.'))
            .unwrap_or(pattern);
        self.suffixes.push(normalize(base));
    }

    /// True if `name` is blocked (exact or under a wildcard suffix).
    pub fn is_blocked(&self, name: &str) -> bool {
        let n = normalize(name);
        if self.exact.contains(&n) {
            return true;
        }
        for suf in &self.suffixes {
            if n == *suf {
                return true;
            }
            // `*.suf`: n ends with ".suf".
            if n.len() > suf.len()
                && n.as_bytes()[n.len() - suf.len() - 1] == b'.'
                && n.ends_with(suf.as_str())
            {
                return true;
            }
        }
        false
    }

    /// Total blocked entries (exact + wildcard).
    pub fn len(&self) -> usize {
        self.exact.len() + self.suffixes.len()
    }

    /// True if nothing is blocked.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.suffixes.is_empty()
    }
}

/// A static hosts table (Linux `/etc/hosts`-shaped): a name -> address mapping
/// checked before the cache and network.
pub struct HostsTable {
    entries: Vec<(String, IpAddr)>,
}

impl Default for HostsTable {
    fn default() -> Self {
        HostsTable::new()
    }
}

impl HostsTable {
    /// An empty table.
    pub fn new() -> HostsTable {
        HostsTable {
            entries: Vec::new(),
        }
    }

    /// Map `name` to `ip`.
    pub fn insert(&mut self, name: &str, ip: IpAddr) {
        self.entries.push((normalize(name), ip));
    }

    /// All addresses for `name` matching `qtype`, or `None` if none.
    pub fn lookup(&self, name: &str, qtype: QType) -> Option<Vec<IpAddr>> {
        let n = normalize(name);
        let mut out = Vec::new();
        for (host, ip) in &self.entries {
            if *host != n {
                continue;
            }
            let hit = matches!(
                (ip, qtype),
                (IpAddr::V4(_), QType::A) | (IpAddr::V6(_), QType::Aaaa)
            );
            if hit {
                out.push(*ip);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

// ---- LRU + TTL cache ----

struct Entry {
    name: String,
    qtype: u16,
    ips: Vec<IpAddr>,
    /// Absolute expiry on the caller's clock; the entry is dead once `now >=`.
    expires_at: u64,
    /// The clock value at the last access (for LRU eviction).
    last_used: u64,
}

/// An LRU + TTL DNS cache keyed on `(name, qtype)`. It works on an opaque
/// monotonic clock supplied by the caller (ticks), so the math is exact and
/// portable. A lookup evicts an expired entry; an insert past `cap` evicts the
/// least-recently-used entry.
pub struct Cache {
    entries: Vec<Entry>,
    cap: usize,
    /// A monotonic counter bumped on each access to order entries for LRU.
    clock: u64,
}

impl Cache {
    /// A cache holding at most `cap` entries (at least 1).
    pub fn new(cap: usize) -> Cache {
        Cache {
            entries: Vec::new(),
            cap: cap.max(1),
            clock: 0,
        }
    }

    fn bump(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Look up `(name, qtype)` at time `now`. Returns the addresses if present
    /// and unexpired (refreshing its LRU position); evicts and returns `None` if
    /// expired.
    pub fn get(&mut self, name: &str, qtype: u16, now: u64) -> Option<Vec<IpAddr>> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.qtype == qtype && e.name == name)?;
        if now >= self.entries[idx].expires_at {
            self.entries.swap_remove(idx);
            return None;
        }
        let u = self.bump();
        self.entries[idx].last_used = u;
        Some(self.entries[idx].ips.clone())
    }

    /// Insert or refresh `(name, qtype)` -> `ips`, expiring at `expires_at` on the
    /// caller's clock. Evicts the LRU entry first if at capacity.
    pub fn insert(&mut self, name: &str, qtype: u16, ips: Vec<IpAddr>, expires_at: u64) {
        let u = self.bump();
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.qtype == qtype && e.name == name)
        {
            e.ips = ips;
            e.expires_at = expires_at;
            e.last_used = u;
            return;
        }
        if self.entries.len() >= self.cap
            && let Some((i, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
        {
            self.entries.swap_remove(i);
        }
        self.entries.push(Entry {
            name: String::from(name),
            qtype,
            ips,
            expires_at,
            last_used: u,
        });
    }

    /// Number of live-or-not entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---- config + resolver (the `hosted` posture: they need the clock + a NIC) ----

/// Resolver configuration: the upstream resolver IPs, the static hosts table, an
/// optional sinkhole address for blocked names, the cache cap, and query timing.
#[cfg(feature = "hosted")]
pub struct Config {
    /// Upstream resolvers, tried in order (each on UDP port 53).
    pub resolvers: Vec<Ipv4Addr>,
    /// The static hosts table (checked before cache + network).
    pub hosts: HostsTable,
    /// If set, a blocked name resolves here instead of `Err(Blocked)`.
    pub sinkhole: Option<IpAddr>,
    /// Nominal clock ticks per second, used to scale a DNS TTL (seconds) to the
    /// cache clock. A calibration, not a measured timebase - TTL-expiry precision
    /// on the live path is best-effort; the deterministic proof never uses it.
    pub ticks_per_sec: u64,
    /// Maximum cache entries.
    pub cache_cap: usize,
    /// Per-attempt reply timeout (via the reactor).
    pub timeout: Duration,
    /// Send/receive attempts before giving up.
    pub retries: u32,
}

#[cfg(feature = "hosted")]
impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

#[cfg(feature = "hosted")]
impl Config {
    /// A default config: no resolvers/hosts, no sinkhole, 256-entry cache, a
    /// 1s per-attempt timeout, 4 retries.
    pub fn new() -> Config {
        Config {
            resolvers: Vec::new(),
            hosts: HostsTable::new(),
            sinkhole: None,
            ticks_per_sec: 1_000_000,
            cache_cap: 256,
            timeout: Duration::from_millis(1_000),
            retries: 4,
        }
    }
}

/// A caching DNS resolver (docs/NETSTACK.md N1c). Resolution checks, in order:
/// the blocklist, the hosts table, the cache (all network-free), then queries the
/// configured resolvers over UDP, caching the answer with its TTL.
#[cfg(feature = "hosted")]
pub struct Resolver {
    config: Config,
    cache: Cache,
    blocklist: Blocklist,
    udp: UdpEndpoint,
    /// Network queries actually sent (the deterministic-proof counter: a blocklist
    /// / hosts / cache hit sends zero).
    queries_sent: u32,
}

#[cfg(feature = "hosted")]
impl Resolver {
    /// A resolver for the local identity `src_mac` / `src_ip` with `config`.
    pub fn new(src_mac: crate::eth::Mac, src_ip: Ipv4Addr, config: Config) -> Resolver {
        let cap = config.cache_cap;
        Resolver {
            udp: UdpEndpoint::new(src_mac, src_ip),
            cache: Cache::new(cap),
            blocklist: Blocklist::new(),
            queries_sent: 0,
            config,
        }
    }

    /// Mutable access to the blocklist (to add blocked names).
    pub fn blocklist_mut(&mut self) -> &mut Blocklist {
        &mut self.blocklist
    }

    /// Mutable access to the config (to add resolvers / hosts).
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    /// The number of network queries sent so far.
    pub fn queries_sent(&self) -> u32 {
        self.queries_sent
    }

    /// Reset the query counter (used to isolate a deterministic assertion).
    pub fn reset_queries(&mut self) {
        self.queries_sent = 0;
    }

    /// Seed the cache directly with a tick-denominated TTL (the deterministic
    /// proof: prove a later lookup is a cache hit without a real network answer).
    pub fn seed_cache(&mut self, name: &str, qtype: QType, ips: Vec<IpAddr>, ttl_ticks: u64) {
        let now = Instant::now().ticks();
        self.cache.insert(
            &normalize(name),
            qtype.as_u16(),
            ips,
            now.saturating_add(ttl_ticks),
        );
    }

    /// Resolve `name` to addresses of `qtype`. Order: blocklist -> hosts -> cache
    /// (all network-free) -> the configured resolvers over UDP. A network answer
    /// is cached with its TTL.
    pub async fn resolve(&mut self, name: &str, qtype: QType) -> Result<Vec<IpAddr>, DnsError> {
        // 1. Blocklist (no network).
        if self.blocklist.is_blocked(name) {
            return match self.config.sinkhole {
                Some(ip) => Ok(alloc::vec![ip]),
                None => Err(DnsError::Blocked),
            };
        }
        let key = normalize(name);
        // 2. Hosts table (no network).
        if let Some(ips) = self.config.hosts.lookup(&key, qtype) {
            return Ok(ips);
        }
        // 3. Cache (no network).
        let now = Instant::now().ticks();
        if let Some(ips) = self.cache.get(&key, qtype.as_u16(), now) {
            return Ok(ips);
        }
        // 4. Network: try each resolver in turn.
        let resolvers = self.config.resolvers.clone();
        if resolvers.is_empty() {
            return Err(DnsError::Timeout);
        }
        for resolver_ip in resolvers {
            match self.query_once(&key, qtype, resolver_ip).await {
                Ok((ips, ttl_secs)) => {
                    if ips.is_empty() {
                        return Err(DnsError::NoAddress);
                    }
                    let now = Instant::now().ticks();
                    let expires = now.saturating_add(
                        (ttl_secs as u64).saturating_mul(self.config.ticks_per_sec),
                    );
                    self.cache
                        .insert(&key, qtype.as_u16(), ips.clone(), expires);
                    return Ok(ips);
                }
                // A timeout on one resolver: try the next.
                Err(DnsError::Timeout) => continue,
                // NXDOMAIN / a hard error is authoritative: stop.
                Err(e) => return Err(e),
            }
        }
        Err(DnsError::Timeout)
    }

    /// Send one query to `resolver` and parse the reply (with retransmits). Bumps
    /// [`queries_sent`](Self::queries_sent) once per send attempt.
    async fn query_once(
        &mut self,
        name: &str,
        qtype: QType,
        resolver: Ipv4Addr,
    ) -> Result<(Vec<IpAddr>, u32), DnsError> {
        let txid = librheo::rng::next_u64() as u16;
        let mut qbuf = [0u8; 512];
        let qlen = build_query(txid, name, qtype, &mut qbuf).ok_or(DnsError::BadName)?;
        // A random ephemeral source port (non-secret randomness from the fast DRBG).
        let src_port = 0xC000 | (librheo::rng::next_u64() as u16 & 0x3FFF);

        let mut reply = [0u8; 1500];
        for _ in 0..self.config.retries.max(1) {
            self.queries_sent += 1;
            match self
                .udp
                .send_to(resolver, 53, src_port, &qbuf[..qlen])
                .await
            {
                Ok(()) => {}
                Err(WireError::Net) => return Err(DnsError::Net),
                Err(_) => continue, // ARP timeout / too big on this attempt - retry
            }
            let got = time::timeout(
                self.config.timeout,
                recv_dns_reply(&mut self.udp, resolver, txid, &mut reply),
            )
            .await;
            match got {
                Ok(Ok(len)) => {
                    let resp = parse_response(&reply[..len])?;
                    if resp.rcode == RCODE_NXDOMAIN {
                        return Err(DnsError::NxDomain);
                    }
                    return Ok(extract(&resp, qtype));
                }
                Ok(Err(DnsError::Net)) => return Err(DnsError::Net),
                // A malformed reply / no matching reply this round: retransmit.
                Ok(Err(_)) => continue,
                // The reactor timeout fired: retransmit.
                Err(time::Elapsed) => continue,
            }
        }
        Err(DnsError::Timeout)
    }
}

/// Poll for a UDP reply from `resolver:53` whose transaction id matches `txid`,
/// copying the DNS message into `buf`; returns its length. Skips datagrams from
/// other sources / with a wrong id, bounded so a chatty backend cannot spin
/// forever. Free function (not a method) so it borrows only the endpoint + buffer.
#[cfg(feature = "hosted")]
async fn recv_dns_reply(
    udp: &mut UdpEndpoint,
    resolver: Ipv4Addr,
    txid: u16,
    buf: &mut [u8],
) -> Result<usize, DnsError> {
    let mut seen = 0u32;
    loop {
        let r = match udp.recv_from(buf).await {
            Ok(r) => r,
            Err(WireError::Net) => return Err(DnsError::Net),
            Err(_) => return Err(DnsError::Timeout),
        };
        seen += 1;
        if seen > 16 {
            return Err(DnsError::Timeout);
        }
        if r.src_ip != resolver || r.src_port != 53 || r.len < 2 {
            continue;
        }
        if u16::from_be_bytes([buf[0], buf[1]]) != txid {
            continue;
        }
        return Ok(r.len);
    }
}
