//! `net::dhcp` - a **DHCP client** (RFC 2131 / RFC 2132), userspace, from scratch
//! (docs/NETSTACK.md, rheo-net Phase N4c). The codec, the
//! DISCOVER -> OFFER -> REQUEST -> ACK state machine, and the lease's T1/T2
//! renewal timers.
//!
//! ## The message shape (and the BOOTP legacy)
//! A DHCP message is a **BOOTP** packet with options bolted on. The fixed part is
//! 236 bytes:
//!
//! ```text
//! [op 1][htype 1][hlen 1][hops 1][xid 4][secs 2][flags 2]
//! [ciaddr 4][yiaddr 4][siaddr 4][giaddr 4][chaddr 16][sname 64][file 128]
//! ```
//!
//! then the four-byte **magic cookie** `63 82 53 63` (RFC 2132 §2 - this is what
//! distinguishes DHCP from plain BOOTP), then TLV options `[code 1][len 1][value]`
//! terminated by `255`. `op` is 1 for a client request and 2 for a server reply;
//! `xid` is the client's transaction id and is how a reply is matched to a request;
//! `yiaddr` ("your address") is the address the server is offering or confirming.
//!
//! Four fields matter for correctness and are easy to get wrong:
//! - **`flags`** bit 15 is the *broadcast* flag. A client with no address yet
//!   cannot receive a unicast reply (it has no address to match), so it must ask
//!   the server to broadcast. [`build_discover`] sets it; a renewal from a bound
//!   client does not.
//! - **`ciaddr`** is *our* address and must be zero until we are bound. In the
//!   RENEWING state it is filled in, and the "requested IP" option must then be
//!   **absent** (RFC 2131 §4.4.5, table 5) - a server may NAK a renewal that
//!   carries both.
//! - **`chaddr`** is the 16-byte client hardware address field holding our 6-byte
//!   MAC and 10 zero bytes. A reply whose `chaddr` prefix is not ours is not for us.
//! - **`xid`** must match, or the message belongs to another client's transaction.
//!
//! ## The addressing special case (why this cannot go through `udp::UdpEndpoint`)
//! Before a lease exists the client has no IP address and no ARP entry for the
//! server, so the first exchange is deliberately degenerate:
//! **`0.0.0.0:68` -> `255.255.255.255:67`**, sent to the Ethernet broadcast MAC with
//! **no ARP resolution at all** (there is nothing to resolve - and nothing to
//! resolve *from*, since ARP needs a sender address). The UDP checksum is computed
//! over a pseudo-header whose source is `0.0.0.0`, which is legal and is what every
//! DHCP client does. The [`hosted`](configure) driver therefore frames through
//! `wire::frame_ipv4` directly rather than through
//! [`UdpEndpoint`](crate::udp::UdpEndpoint), whose whole job is next-hop
//! resolution.
//!
//! ## The lease and its timers
//! An ACK carries a lease length plus, usually, **T1** (renewal) and **T2**
//! (rebinding). When absent they default to RFC 2131 §4.4.5's values: `T1 = 0.5 *
//! lease`, `T2 = 0.875 * lease`. The client then walks:
//!
//! ```text
//! BOUND --T1--> RENEWING (unicast REQUEST to the leasing server)
//!       --T2--> REBINDING (broadcast REQUEST to any server)
//!       --lease expiry--> back to SELECTING (address dropped, DISCOVER)
//! ```
//!
//! An ACK in RENEWING or REBINDING re-arms all three timers from the moment the ACK
//! arrived. A NAK anywhere drops the lease and restarts at DISCOVER.
//!
//! ## Two more messages that matter operationally
//! - [`Client::decline`] sends **DECLINE** when the offered address turns out to be
//!   already in use (found by ARP-probing it, the same probe
//!   [`crate::zeroconf`] uses) and restarts. Skipping this is how a duplicate
//!   address silently persists on a link.
//! - [`Client::release`] sends **RELEASE** so the server can reuse the address
//!   immediately instead of waiting out the lease.
//!
//! ## Postures
//! The codec and the [`Client`] state machine are **always compiled** - they are
//! pure byte handling plus integer timers driven by an explicit `now_ns`, which is
//! exactly what makes the deterministic proof possible (craft an OFFER, feed it,
//! advance the clock by hand). Only [`configure`] - the async loop that actually
//! puts frames on the NIC - is behind the `hosted` feature.
//!
//! ## Deferred (documented)
//! DHCPv6 and IPv6 SLAAC; INFORM; the relay-agent (`giaddr`) path; option overload
//! (`sname`/`file` carrying options, RFC 2132 §9.3); classless static routes
//! (option 121); a DHCP **server**; and persisting a lease across a reboot so it
//! can be re-REQUESTed (the INIT-REBOOT state). Only the client states listed above
//! are implemented.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ip::Ipv4Addr;

/// The DHCP server port.
pub const SERVER_PORT: u16 = 67;
/// The DHCP client port.
pub const CLIENT_PORT: u16 = 68;

/// The fixed (BOOTP) part of a DHCP message, in bytes - everything before the
/// magic cookie.
pub const FIXED_LEN: usize = 236;
/// The magic cookie that marks a BOOTP packet as carrying DHCP options (RFC 2132
/// §2).
pub const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// The smallest possible valid message: the fixed part plus the cookie.
pub const MIN_LEN: usize = FIXED_LEN + 4;
/// The length every message this client builds is padded to. RFC 2131 §2 notes
/// implementations that cannot handle a message shorter than 300 octets, and
/// padding to it costs nothing.
pub const PADDED_LEN: usize = 300;

/// `op`: a client -> server message.
pub const BOOTREQUEST: u8 = 1;
/// `op`: a server -> client message.
pub const BOOTREPLY: u8 = 2;
/// `htype`: Ethernet.
pub const HTYPE_ETHERNET: u8 = 1;
/// `hlen`: an Ethernet MAC is 6 bytes.
pub const HLEN_ETHERNET: u8 = 6;
/// `flags` bit 15: "please broadcast your reply" (RFC 2131 §2).
pub const FLAG_BROADCAST: u16 = 0x8000;

/// DHCP message types (option 53).
pub mod msg {
    /// A client asking any server for an address.
    pub const DISCOVER: u8 = 1;
    /// A server offering one.
    pub const OFFER: u8 = 2;
    /// A client accepting an offer, or renewing/rebinding a lease.
    pub const REQUEST: u8 = 3;
    /// A client rejecting an address it found already in use.
    pub const DECLINE: u8 = 4;
    /// A server confirming a lease.
    pub const ACK: u8 = 5;
    /// A server refusing (the address is gone / the request is wrong).
    pub const NAK: u8 = 6;
    /// A client giving an address back.
    pub const RELEASE: u8 = 7;
    /// A client asking only for configuration, keeping its own address.
    pub const INFORM: u8 = 8;
}

/// DHCP option codes (RFC 2132) - the ones this client uses.
pub mod opt {
    /// Padding; carries no length byte.
    pub const PAD: u8 = 0;
    /// The subnet mask.
    pub const SUBNET_MASK: u8 = 1;
    /// The default gateway(s); the first is used.
    pub const ROUTER: u8 = 3;
    /// The DNS server list.
    pub const DNS_SERVER: u8 = 6;
    /// The client's hostname.
    pub const HOSTNAME: u8 = 12;
    /// The domain name (used as a search domain).
    pub const DOMAIN_NAME: u8 = 15;
    /// The address the client is asking for.
    pub const REQUESTED_IP: u8 = 50;
    /// The lease length, in seconds.
    pub const LEASE_TIME: u8 = 51;
    /// The message type ([`super::msg`]).
    pub const MSG_TYPE: u8 = 53;
    /// The server's identifying address.
    pub const SERVER_ID: u8 = 54;
    /// The list of option codes the client would like returned.
    pub const PARAM_REQUEST_LIST: u8 = 55;
    /// A human-readable error from the server (accompanies a NAK).
    pub const MESSAGE: u8 = 56;
    /// T1: when to start renewing.
    pub const RENEWAL_T1: u8 = 58;
    /// T2: when to start rebinding.
    pub const REBINDING_T2: u8 = 59;
    /// End of the option block; carries no length byte.
    pub const END: u8 = 255;
}

/// The option codes this client asks the server to include (option 55). Keeping
/// this a named constant means the DISCOVER byte oracle in the proof pins it.
pub const PARAM_REQUESTS: [u8; 4] = [
    opt::SUBNET_MASK,
    opt::ROUTER,
    opt::DNS_SERVER,
    opt::DOMAIN_NAME,
];

/// Nanoseconds in a second - the timers are held in ns (the clock the reactor and
/// [`crate::timer::TimerWheel`] speak) while the wire carries seconds.
const NS_PER_SEC: u64 = 1_000_000_000;

/// Why a DHCP message was rejected, or a client operation refused. Every shape is
/// its own value: a proof that asserts "rejected" without asserting *why* would
/// pass on the wrong rejection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DhcpError {
    /// Shorter than the fixed part plus the cookie.
    TooShort,
    /// The four magic-cookie bytes are not `63 82 53 63` - this is BOOTP, not DHCP.
    BadCookie,
    /// An option's length byte runs past the end of the message.
    TruncatedOption,
    /// No message-type option (53) - every DHCP message must carry one.
    NoMessageType,
    /// `op` is not [`BOOTREPLY`] - a client only ever consumes server replies.
    NotAReply,
    /// The transaction id is not ours.
    XidMismatch,
    /// The `chaddr` prefix is some other client's MAC.
    NotOurMac,
    /// An option's value has the wrong length for its code (a 3-byte subnet mask).
    BadOptionLength,
    /// An ACK arrived with no address in `yiaddr`, or no lease-time option.
    IncompleteLease,
    /// The operation needs a lease and there is none.
    NoLease,
    /// The raw-frame transport failed (the `hosted` driver only).
    Net,
    /// No reply arrived within the retry/timeout budget (the `hosted` driver only).
    Timeout,
}

// ---- options ----

/// A borrowed view of a message's option block. Iterating it is the only way
/// options are read, so the truncation check lives in exactly one place.
#[derive(Copy, Clone, Debug)]
pub struct Options<'a>(&'a [u8]);

impl<'a> Options<'a> {
    /// The raw option bytes (after the magic cookie).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    /// Iterate `(code, value)` pairs. `PAD` is skipped, `END` stops the walk, and a
    /// length byte that would run past the end yields [`DhcpError::TruncatedOption`].
    pub fn iter(&self) -> OptionIter<'a> {
        OptionIter {
            buf: self.0,
            pos: 0,
        }
    }

    /// The value of option `code`, or `None`. The **first** occurrence wins; a
    /// truncated block yields `None` (use [`Self::validate`] to distinguish
    /// "absent" from "malformed").
    pub fn get(&self, code: u8) -> Option<&'a [u8]> {
        for item in self.iter() {
            match item {
                Ok((c, v)) if c == code => return Some(v),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// Walk the whole block, returning the first structural error.
    pub fn validate(&self) -> Result<(), DhcpError> {
        for item in self.iter() {
            item?;
        }
        Ok(())
    }

    /// Option `code` as a single byte (a message type), if present and 1 byte long.
    pub fn get_u8(&self, code: u8) -> Option<u8> {
        match self.get(code)? {
            [b] => Some(*b),
            _ => None,
        }
    }

    /// Option `code` as a big-endian `u32` (a lease/T1/T2 time), if present and
    /// 4 bytes long.
    pub fn get_u32(&self, code: u8) -> Option<u32> {
        match self.get(code)? {
            [a, b, c, d] => Some(u32::from_be_bytes([*a, *b, *c, *d])),
            _ => None,
        }
    }

    /// Option `code` as one IPv4 address (the first, if the option holds several).
    pub fn get_ipv4(&self, code: u8) -> Option<Ipv4Addr> {
        let v = self.get(code)?;
        if v.len() < 4 {
            return None;
        }
        Some(Ipv4Addr([v[0], v[1], v[2], v[3]]))
    }

    /// Option `code` as a list of IPv4 addresses (the DNS server list). A trailing
    /// partial address is ignored.
    pub fn get_ipv4_list(&self, code: u8) -> Vec<Ipv4Addr> {
        let mut out = Vec::new();
        if let Some(v) = self.get(code) {
            // `as_chunks` gives fixed-size `[u8; 4]` groups plus the trailing
            // remainder, which we drop (a partial address is not an address).
            let (addrs, _partial) = v.as_chunks::<4>();
            for &octets in addrs {
                out.push(Ipv4Addr(octets));
            }
        }
        out
    }

    /// Option `code` as a string (the domain name / hostname). Non-UTF-8 bytes are
    /// dropped rather than failing the whole message - a bad domain name should not
    /// cost a working lease.
    pub fn get_str(&self, code: u8) -> Option<String> {
        let v = self.get(code)?;
        // A server may NUL-terminate; trim any trailing NULs.
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        core::str::from_utf8(&v[..end]).ok().map(String::from)
    }
}

/// The iterator [`Options::iter`] returns.
pub struct OptionIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for OptionIter<'a> {
    type Item = Result<(u8, &'a [u8]), DhcpError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.buf.len() {
                return None; // ran out without an END: treat as the end
            }
            let code = self.buf[self.pos];
            if code == opt::PAD {
                self.pos += 1;
                continue;
            }
            if code == opt::END {
                self.pos = self.buf.len();
                return None;
            }
            if self.pos + 1 >= self.buf.len() {
                self.pos = self.buf.len();
                return Some(Err(DhcpError::TruncatedOption));
            }
            let len = self.buf[self.pos + 1] as usize;
            let start = self.pos + 2;
            let end = start + len;
            if end > self.buf.len() {
                self.pos = self.buf.len();
                return Some(Err(DhcpError::TruncatedOption));
            }
            self.pos = end;
            return Some(Ok((code, &self.buf[start..end])));
        }
    }
}

// ---- the message codec ----

/// A parsed DHCP message: the BOOTP fields that matter plus a borrowed view of the
/// option block.
#[derive(Copy, Clone, Debug)]
pub struct Message<'a> {
    /// `op`: [`BOOTREQUEST`] or [`BOOTREPLY`].
    pub op: u8,
    /// The transaction id.
    pub xid: u32,
    /// Seconds since the client began the exchange.
    pub secs: u16,
    /// The flags word ([`FLAG_BROADCAST`]).
    pub flags: u16,
    /// The client's own address (zero until bound).
    pub ciaddr: Ipv4Addr,
    /// "Your address" - the address being offered/confirmed.
    pub yiaddr: Ipv4Addr,
    /// The next-server address (BOOTP boot-file loading; unused here).
    pub siaddr: Ipv4Addr,
    /// The relay-agent address (zero on a directly-attached link).
    pub giaddr: Ipv4Addr,
    /// The client MAC (the first 6 bytes of the 16-byte `chaddr` field).
    pub chaddr: [u8; 6],
    /// The option block.
    pub options: Options<'a>,
}

impl<'a> Message<'a> {
    /// The message type (option 53).
    pub fn msg_type(&self) -> Result<u8, DhcpError> {
        self.options
            .get_u8(opt::MSG_TYPE)
            .ok_or(DhcpError::NoMessageType)
    }
}

fn ipv4_at(buf: &[u8], off: usize) -> Ipv4Addr {
    Ipv4Addr([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Parse a DHCP message. Checks the length and the magic cookie, and **validates
/// the whole option block** so a truncated TLV is rejected here rather than
/// silently ignored by a later `get`.
pub fn parse(buf: &[u8]) -> Result<Message<'_>, DhcpError> {
    if buf.len() < MIN_LEN {
        return Err(DhcpError::TooShort);
    }
    if buf[FIXED_LEN..FIXED_LEN + 4] != MAGIC_COOKIE {
        return Err(DhcpError::BadCookie);
    }
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&buf[28..34]);
    let options = Options(&buf[FIXED_LEN + 4..]);
    options.validate()?;
    Ok(Message {
        op: buf[0],
        xid: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        secs: u16::from_be_bytes([buf[8], buf[9]]),
        flags: u16::from_be_bytes([buf[10], buf[11]]),
        ciaddr: ipv4_at(buf, 12),
        yiaddr: ipv4_at(buf, 16),
        siaddr: ipv4_at(buf, 20),
        giaddr: ipv4_at(buf, 24),
        chaddr,
        options,
    })
}

/// The fields a message builder needs. Grouped so the builders stay short and so
/// the client and the (test-side) server encoders share one shape.
#[derive(Copy, Clone, Debug)]
pub struct Header {
    /// [`BOOTREQUEST`] or [`BOOTREPLY`].
    pub op: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub mac: [u8; 6],
}

impl Header {
    /// A client request header with everything zero but `xid`/`secs`/`mac`.
    pub fn request(xid: u32, secs: u16, mac: [u8; 6]) -> Header {
        Header {
            op: BOOTREQUEST,
            xid,
            secs,
            flags: 0,
            ciaddr: crate::hostcfg::UNSPECIFIED,
            yiaddr: crate::hostcfg::UNSPECIFIED,
            siaddr: crate::hostcfg::UNSPECIFIED,
            mac,
        }
    }
}

/// Start a message: write the 236-byte fixed part plus the magic cookie into a
/// fresh `Vec`, leaving the option block to the caller (which must finish with
/// [`finish`]).
fn begin(h: &Header) -> Vec<u8> {
    let mut m = alloc::vec![0u8; MIN_LEN];
    m[0] = h.op;
    m[1] = HTYPE_ETHERNET;
    m[2] = HLEN_ETHERNET;
    m[3] = 0; // hops
    m[4..8].copy_from_slice(&h.xid.to_be_bytes());
    m[8..10].copy_from_slice(&h.secs.to_be_bytes());
    m[10..12].copy_from_slice(&h.flags.to_be_bytes());
    m[12..16].copy_from_slice(&h.ciaddr.0);
    m[16..20].copy_from_slice(&h.yiaddr.0);
    m[20..24].copy_from_slice(&h.siaddr.0);
    // giaddr stays zero: no relay agent.
    m[28..34].copy_from_slice(&h.mac);
    // sname / file stay zero.
    m[FIXED_LEN..MIN_LEN].copy_from_slice(&MAGIC_COOKIE);
    m
}

/// Append one TLV option.
fn push_opt(m: &mut Vec<u8>, code: u8, value: &[u8]) {
    m.push(code);
    m.push(value.len() as u8);
    m.extend_from_slice(value);
}

/// Terminate the option block with `END` and pad to [`PADDED_LEN`].
fn finish(mut m: Vec<u8>) -> Vec<u8> {
    m.push(opt::END);
    while m.len() < PADDED_LEN {
        m.push(opt::PAD);
    }
    m
}

/// Build a **DISCOVER**: "any server, please offer me an address". Broadcast flag
/// set (we have no address to receive a unicast reply at), the parameter-request
/// list asking for mask/router/DNS/domain, plus an optional preferred address and
/// hostname.
pub fn build_discover(
    xid: u32,
    mac: [u8; 6],
    secs: u16,
    requested: Option<Ipv4Addr>,
    hostname: Option<&str>,
) -> Vec<u8> {
    let mut h = Header::request(xid, secs, mac);
    h.flags = FLAG_BROADCAST;
    let mut m = begin(&h);
    push_opt(&mut m, opt::MSG_TYPE, &[msg::DISCOVER]);
    push_opt(&mut m, opt::PARAM_REQUEST_LIST, &PARAM_REQUESTS);
    if let Some(ip) = requested {
        push_opt(&mut m, opt::REQUESTED_IP, &ip.0);
    }
    if let Some(n) = hostname {
        push_opt(&mut m, opt::HOSTNAME, n.as_bytes());
    }
    finish(m)
}

/// Build a **REQUEST**.
///
/// Two shapes, distinguished exactly as RFC 2131 §4.4.5 table 5 requires:
/// - *selecting* / *rebinding* (`ciaddr` zero): the address goes in the
///   **requested-IP** option, and a selecting REQUEST also echoes the
///   **server-id** so other servers know their offer was declined. Broadcast.
/// - *renewing* (`ciaddr` set): the address is in `ciaddr`, and the requested-IP
///   and server-id options are **omitted**. Unicast to the leasing server.
pub fn build_request(
    xid: u32,
    mac: [u8; 6],
    secs: u16,
    ciaddr: Ipv4Addr,
    requested: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
    hostname: Option<&str>,
) -> Vec<u8> {
    let mut h = Header::request(xid, secs, mac);
    h.ciaddr = ciaddr;
    if ciaddr == crate::hostcfg::UNSPECIFIED {
        h.flags = FLAG_BROADCAST;
    }
    let mut m = begin(&h);
    push_opt(&mut m, opt::MSG_TYPE, &[msg::REQUEST]);
    if let Some(sid) = server_id {
        push_opt(&mut m, opt::SERVER_ID, &sid.0);
    }
    if let Some(ip) = requested {
        push_opt(&mut m, opt::REQUESTED_IP, &ip.0);
    }
    push_opt(&mut m, opt::PARAM_REQUEST_LIST, &PARAM_REQUESTS);
    if let Some(n) = hostname {
        push_opt(&mut m, opt::HOSTNAME, n.as_bytes());
    }
    finish(m)
}

/// Build a **DECLINE**: the offered address is already in use (an ARP probe
/// answered), so refuse it and tell the server which address and which server.
pub fn build_decline(xid: u32, mac: [u8; 6], declined: Ipv4Addr, server_id: Ipv4Addr) -> Vec<u8> {
    let mut h = Header::request(xid, 0, mac);
    h.flags = FLAG_BROADCAST;
    let mut m = begin(&h);
    push_opt(&mut m, opt::MSG_TYPE, &[msg::DECLINE]);
    push_opt(&mut m, opt::SERVER_ID, &server_id.0);
    push_opt(&mut m, opt::REQUESTED_IP, &declined.0);
    finish(m)
}

/// Build a **RELEASE**: hand the address back so the server can reuse it. Unicast
/// to the leasing server with `ciaddr` set (RFC 2131 §4.4.6).
pub fn build_release(xid: u32, mac: [u8; 6], address: Ipv4Addr, server_id: Ipv4Addr) -> Vec<u8> {
    let mut h = Header::request(xid, 0, mac);
    h.ciaddr = address;
    let mut m = begin(&h);
    push_opt(&mut m, opt::MSG_TYPE, &[msg::RELEASE]);
    push_opt(&mut m, opt::SERVER_ID, &server_id.0);
    finish(m)
}

/// What a server would send back. Fields a test (or, later, a DHCP **server**)
/// fills to encode an OFFER / ACK / NAK with [`build_reply`].
#[derive(Clone, Debug)]
pub struct ReplyParams {
    /// [`msg::OFFER`], [`msg::ACK`] or [`msg::NAK`].
    pub msg_type: u8,
    pub xid: u32,
    pub mac: [u8; 6],
    /// The address being offered/confirmed (zero in a NAK).
    pub yiaddr: Ipv4Addr,
    pub server_id: Ipv4Addr,
    pub netmask: Option<Ipv4Addr>,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    pub hostname: Option<String>,
    pub lease_secs: Option<u32>,
    pub t1_secs: Option<u32>,
    pub t2_secs: Option<u32>,
    /// A human-readable reason (option 56), typically with a NAK.
    pub message: Option<String>,
}

impl ReplyParams {
    /// An OFFER/ACK skeleton: the type, ids and address, no options yet.
    pub fn new(
        msg_type: u8,
        xid: u32,
        mac: [u8; 6],
        yiaddr: Ipv4Addr,
        server_id: Ipv4Addr,
    ) -> ReplyParams {
        ReplyParams {
            msg_type,
            xid,
            mac,
            yiaddr,
            server_id,
            netmask: None,
            router: None,
            dns: Vec::new(),
            domain: None,
            hostname: None,
            lease_secs: None,
            t1_secs: None,
            t2_secs: None,
            message: None,
        }
    }
}

/// Encode a server reply (OFFER / ACK / NAK).
///
/// This is the **server** side of the codec. It exists for two reasons: the
/// deterministic proof crafts its OFFER and ACK with it, so encode *and* decode are
/// both exercised on the same bytes rather than against hand-typed hex; and a DHCP
/// server cell, when it is built, needs exactly this function.
pub fn build_reply(p: &ReplyParams) -> Vec<u8> {
    let h = Header {
        op: BOOTREPLY,
        xid: p.xid,
        secs: 0,
        flags: 0,
        ciaddr: crate::hostcfg::UNSPECIFIED,
        yiaddr: p.yiaddr,
        siaddr: p.server_id,
        mac: p.mac,
    };
    let mut m = begin(&h);
    push_opt(&mut m, opt::MSG_TYPE, &[p.msg_type]);
    push_opt(&mut m, opt::SERVER_ID, &p.server_id.0);
    if let Some(v) = p.netmask {
        push_opt(&mut m, opt::SUBNET_MASK, &v.0);
    }
    if let Some(v) = p.router {
        push_opt(&mut m, opt::ROUTER, &v.0);
    }
    if !p.dns.is_empty() {
        let mut bytes = Vec::with_capacity(p.dns.len() * 4);
        for ip in &p.dns {
            bytes.extend_from_slice(&ip.0);
        }
        push_opt(&mut m, opt::DNS_SERVER, &bytes);
    }
    if let Some(v) = &p.domain {
        push_opt(&mut m, opt::DOMAIN_NAME, v.as_bytes());
    }
    if let Some(v) = &p.hostname {
        push_opt(&mut m, opt::HOSTNAME, v.as_bytes());
    }
    if let Some(v) = p.lease_secs {
        push_opt(&mut m, opt::LEASE_TIME, &v.to_be_bytes());
    }
    if let Some(v) = p.t1_secs {
        push_opt(&mut m, opt::RENEWAL_T1, &v.to_be_bytes());
    }
    if let Some(v) = p.t2_secs {
        push_opt(&mut m, opt::REBINDING_T2, &v.to_be_bytes());
    }
    if let Some(v) = &p.message {
        push_opt(&mut m, opt::MESSAGE, v.as_bytes());
    }
    finish(m)
}

// ---- the lease ----

/// A lease won from a server: the address and everything that came with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The leased address (`yiaddr`).
    pub address: Ipv4Addr,
    /// The subnet mask (option 1), if the server sent one.
    pub netmask: Option<Ipv4Addr>,
    /// The default gateway (option 3, first entry).
    pub router: Option<Ipv4Addr>,
    /// The DNS servers (option 6), in order.
    pub dns: Vec<Ipv4Addr>,
    /// The domain name (option 15) - a search domain.
    pub domain: Option<String>,
    /// The hostname the server assigned (option 12), if any.
    pub hostname: Option<String>,
    /// The server that granted it (option 54) - where a renewal is unicast.
    pub server_id: Ipv4Addr,
    /// The lease length in seconds (option 51).
    pub lease_secs: u32,
    /// T1, the renewal time in seconds (option 58, or `lease/2`).
    pub t1_secs: u32,
    /// T2, the rebinding time in seconds (option 59, or `lease * 7/8`).
    pub t2_secs: u32,
}

impl Lease {
    /// Build a lease from an ACK/OFFER. Fills T1/T2 from RFC 2131 §4.4.5's defaults
    /// when the server omitted them, and clamps a nonsensical pair (`T1 >= T2` or
    /// either past the lease) back onto the defaults - a server that sends
    /// `T1 > T2` would otherwise make the client skip RENEWING entirely.
    pub fn from_message(m: &Message<'_>) -> Result<Lease, DhcpError> {
        if m.yiaddr == crate::hostcfg::UNSPECIFIED {
            return Err(DhcpError::IncompleteLease);
        }
        let lease_secs = m
            .options
            .get_u32(opt::LEASE_TIME)
            .ok_or(DhcpError::IncompleteLease)?;
        if m.options.get(opt::SUBNET_MASK).is_some()
            && m.options.get_ipv4(opt::SUBNET_MASK).is_none()
        {
            return Err(DhcpError::BadOptionLength);
        }
        let server_id = m
            .options
            .get_ipv4(opt::SERVER_ID)
            .unwrap_or(crate::hostcfg::UNSPECIFIED);
        // Defaults: half the lease, then seven-eighths of it.
        let default_t1 = lease_secs / 2;
        let default_t2 = (lease_secs / 8).saturating_mul(7);
        let mut t1 = m.options.get_u32(opt::RENEWAL_T1).unwrap_or(default_t1);
        let mut t2 = m.options.get_u32(opt::REBINDING_T2).unwrap_or(default_t2);
        if t1 == 0 || t1 >= lease_secs {
            t1 = default_t1;
        }
        if t2 <= t1 || t2 > lease_secs {
            t2 = default_t2.max(t1.saturating_add(1)).min(lease_secs);
        }
        Ok(Lease {
            address: m.yiaddr,
            netmask: m.options.get_ipv4(opt::SUBNET_MASK),
            router: m.options.get_ipv4(opt::ROUTER),
            dns: m.options.get_ipv4_list(opt::DNS_SERVER),
            domain: m.options.get_str(opt::DOMAIN_NAME),
            hostname: m.options.get_str(opt::HOSTNAME),
            server_id,
            lease_secs,
            t1_secs: t1,
            t2_secs: t2,
        })
    }
}

// ---- the client state machine ----

/// The client's DHCP state (RFC 2131 §4.4's state machine, client side).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing has been sent yet.
    Init,
    /// A DISCOVER is out; waiting for an OFFER.
    Selecting,
    /// An offer was accepted with a REQUEST; waiting for an ACK.
    Requesting,
    /// A lease is held and current.
    Bound,
    /// Past T1: renewing by unicast to the leasing server.
    Renewing,
    /// Past T2: rebinding by broadcast to any server.
    Rebinding,
}

/// A datagram the client wants sent, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Send from `0.0.0.0:68` to `255.255.255.255:67` on the Ethernet broadcast
    /// MAC, with **no ARP** - see the module docs.
    Broadcast(Vec<u8>),
    /// Send from our leased address to `to:67`, resolving `to`'s MAC normally.
    Unicast {
        /// The leasing server.
        to: Ipv4Addr,
        /// The message.
        data: Vec<u8>,
    },
}

impl Output {
    /// The message bytes, whichever shape this is.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Output::Broadcast(v) => v,
            Output::Unicast { data, .. } => data,
        }
    }
}

/// What feeding a message to the client meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An OFFER was accepted; a REQUEST is going out for this address.
    Offered(Ipv4Addr),
    /// An ACK confirmed a **new** lease - the client is now BOUND.
    Bound,
    /// An ACK confirmed an **existing** lease (a renewal or rebind succeeded).
    Renewed,
    /// The server said no. The lease (if any) is dropped and a DISCOVER goes out.
    Nak,
    /// A well-formed message that this state has nothing to do with (a second
    /// OFFER after one was already accepted, an ACK while unbound).
    Ignored,
}

/// A timer firing changed the client's state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TimerEvent {
    /// T1 passed: BOUND -> RENEWING.
    Renewing,
    /// T2 passed: RENEWING -> REBINDING.
    Rebinding,
    /// The lease ran out: the address is dropped and the client restarts at
    /// SELECTING.
    Expired,
}

/// A DHCP client. Drive it three ways, all with an explicit `now_ns`:
/// [`start`](Self::start) to begin, [`on_message`](Self::on_message) for each
/// received datagram, and [`poll_timers`](Self::poll_timers) whenever the clock
/// advances. Every one of them hands back the datagram to send, so the transport
/// stays entirely outside.
pub struct Client {
    mac: [u8; 6],
    hostname: Option<String>,
    state: State,
    xid: u32,
    /// Splitmix64 state for transaction ids. **Public randomness only** - an xid is
    /// an anti-crosstalk tag, not a secret, so the fast generator is correct here
    /// (docs/NETSTACK.md §3's two-randomness-class rule: key material never comes
    /// from this).
    rng: u64,
    /// When the current exchange began, for the `secs` field.
    started_ns: u64,
    /// The address an OFFER proposed, while REQUESTING.
    offered: Option<Ipv4Addr>,
    /// The server that made the offer, while REQUESTING.
    offered_server: Option<Ipv4Addr>,
    lease: Option<Lease>,
    t1_ns: u64,
    t2_ns: u64,
    expires_ns: u64,
    /// Messages sent (the proof's "did it really transmit?" counter).
    sent: u32,
}

/// Splitmix64 - a small, fast, well-distributed generator for **non-secret**
/// values (DHCP transaction ids, link-local address candidates, backoff jitter).
/// Never used for key material; that is `crypto::kdf` over the attested per-cell
/// DRBG (docs/NETSTACK.md §3).
pub(crate) fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Client {
    /// A fresh client for `mac`, with `seed` seeding the transaction-id generator
    /// (pass a fixed seed for a reproducible test, `librheo::rng::next_u64()` in a
    /// cell).
    pub fn new(mac: [u8; 6], seed: u64) -> Client {
        Client {
            mac,
            hostname: None,
            state: State::Init,
            xid: 0,
            rng: seed,
            started_ns: 0,
            offered: None,
            offered_server: None,
            lease: None,
            t1_ns: 0,
            t2_ns: 0,
            expires_ns: 0,
            sent: 0,
        }
    }

    /// Set the hostname to advertise (option 12).
    pub fn set_hostname(&mut self, name: &str) {
        self.hostname = Some(String::from(name));
    }

    /// The current state.
    pub fn state(&self) -> State {
        self.state
    }

    /// The lease, once bound.
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// The current transaction id.
    pub fn xid(&self) -> u32 {
        self.xid
    }

    /// Absolute T1 deadline, in the caller's ns clock (0 while unbound).
    pub fn t1_ns(&self) -> u64 {
        self.t1_ns
    }

    /// Absolute T2 deadline (0 while unbound).
    pub fn t2_ns(&self) -> u64 {
        self.t2_ns
    }

    /// Absolute lease-expiry deadline (0 while unbound).
    pub fn expires_ns(&self) -> u64 {
        self.expires_ns
    }

    /// The nearest deadline the client wants to be woken at, for a
    /// [`crate::timer::TimerWheel`]. `None` while it holds no lease.
    pub fn next_deadline_ns(&self) -> Option<u64> {
        match self.state {
            State::Bound => Some(self.t1_ns),
            State::Renewing => Some(self.t2_ns),
            State::Rebinding => Some(self.expires_ns),
            _ => None,
        }
    }

    /// How many messages the client has emitted.
    pub fn sent(&self) -> u32 {
        self.sent
    }

    /// Begin (or restart) the exchange: a new transaction id and a broadcast
    /// DISCOVER. State becomes [`State::Selecting`].
    pub fn start(&mut self, now_ns: u64) -> Output {
        self.xid = splitmix64(&mut self.rng) as u32;
        self.started_ns = now_ns;
        self.state = State::Selecting;
        self.offered = None;
        self.offered_server = None;
        self.discover(now_ns)
    }

    /// Re-send the current DISCOVER (a retransmit; same xid).
    pub fn discover(&mut self, now_ns: u64) -> Output {
        self.sent += 1;
        Output::Broadcast(build_discover(
            self.xid,
            self.mac,
            self.secs(now_ns),
            None,
            self.hostname.as_deref(),
        ))
    }

    /// Seconds since the exchange began, saturating into the 16-bit wire field.
    fn secs(&self, now_ns: u64) -> u16 {
        let elapsed = now_ns.saturating_sub(self.started_ns) / NS_PER_SEC;
        elapsed.min(u16::MAX as u64) as u16
    }

    /// Feed a received datagram (the UDP **payload**, i.e. the DHCP message).
    ///
    /// Rejections come first and are specific: too short, bad cookie, a truncated
    /// option, not a reply, a foreign transaction id, another client's MAC. Then the
    /// message type is dispatched against the current state. Returns the event and,
    /// when the transition needs one, the datagram to send.
    pub fn on_message(
        &mut self,
        buf: &[u8],
        now_ns: u64,
    ) -> Result<(Event, Option<Output>), DhcpError> {
        let m = parse(buf)?;
        if m.op != BOOTREPLY {
            return Err(DhcpError::NotAReply);
        }
        if m.xid != self.xid {
            return Err(DhcpError::XidMismatch);
        }
        if m.chaddr != self.mac {
            return Err(DhcpError::NotOurMac);
        }
        let mtype = m.msg_type()?;
        match (mtype, self.state) {
            (msg::OFFER, State::Selecting) => {
                if m.yiaddr == crate::hostcfg::UNSPECIFIED {
                    return Err(DhcpError::IncompleteLease);
                }
                let server = m.options.get_ipv4(opt::SERVER_ID);
                self.offered = Some(m.yiaddr);
                self.offered_server = server;
                self.state = State::Requesting;
                self.sent += 1;
                let out = Output::Broadcast(build_request(
                    self.xid,
                    self.mac,
                    self.secs(now_ns),
                    crate::hostcfg::UNSPECIFIED,
                    Some(m.yiaddr),
                    server,
                    self.hostname.as_deref(),
                ));
                Ok((Event::Offered(m.yiaddr), Some(out)))
            }
            (msg::ACK, State::Requesting | State::Renewing | State::Rebinding) => {
                let renewal = matches!(self.state, State::Renewing | State::Rebinding);
                let lease = Lease::from_message(&m)?;
                self.arm(&lease, now_ns);
                self.lease = Some(lease);
                self.state = State::Bound;
                self.offered = None;
                self.offered_server = None;
                Ok((
                    if renewal {
                        Event::Renewed
                    } else {
                        Event::Bound
                    },
                    None,
                ))
            }
            (msg::NAK, State::Requesting | State::Renewing | State::Rebinding) => {
                self.lease = None;
                self.t1_ns = 0;
                self.t2_ns = 0;
                self.expires_ns = 0;
                let out = self.start(now_ns);
                Ok((Event::Nak, Some(out)))
            }
            _ => Ok((Event::Ignored, None)),
        }
    }

    /// Set the three absolute deadlines from a lease taken at `now_ns`.
    fn arm(&mut self, lease: &Lease, now_ns: u64) {
        let ns = |secs: u32| now_ns.saturating_add((secs as u64).saturating_mul(NS_PER_SEC));
        self.t1_ns = ns(lease.t1_secs);
        self.t2_ns = ns(lease.t2_secs);
        self.expires_ns = ns(lease.lease_secs);
    }

    /// Advance the clock to `now_ns` and take whatever transition is due.
    ///
    /// Expiry is checked **first**, so a clock jump past the whole lease drops the
    /// address rather than trying to renew a dead lease. Returns `None` if nothing
    /// is due.
    pub fn poll_timers(&mut self, now_ns: u64) -> Option<(TimerEvent, Output)> {
        let lease = self.lease.clone()?;
        if now_ns >= self.expires_ns {
            // The lease is gone: drop the address and start over.
            self.lease = None;
            self.t1_ns = 0;
            self.t2_ns = 0;
            self.expires_ns = 0;
            let out = self.start(now_ns);
            return Some((TimerEvent::Expired, out));
        }
        match self.state {
            State::Bound if now_ns >= self.t1_ns => {
                self.state = State::Renewing;
                self.started_ns = now_ns;
                self.sent += 1;
                // RENEWING: ciaddr set, requested-IP and server-id omitted,
                // unicast to the leasing server (RFC 2131 §4.4.5).
                let out = Output::Unicast {
                    to: lease.server_id,
                    data: build_request(
                        self.xid,
                        self.mac,
                        0,
                        lease.address,
                        None,
                        None,
                        self.hostname.as_deref(),
                    ),
                };
                Some((TimerEvent::Renewing, out))
            }
            State::Renewing if now_ns >= self.t2_ns => {
                self.state = State::Rebinding;
                self.started_ns = now_ns;
                self.sent += 1;
                // REBINDING: broadcast to any server, still with ciaddr set.
                let out = Output::Broadcast(build_request(
                    self.xid,
                    self.mac,
                    0,
                    lease.address,
                    None,
                    None,
                    self.hostname.as_deref(),
                ));
                Some((TimerEvent::Rebinding, out))
            }
            _ => None,
        }
    }

    /// Refuse the address currently offered or held because a **conflict** was
    /// found (an ARP probe for it was answered). Emits a DECLINE and drops back to
    /// [`State::Init`]; the caller then calls [`start`](Self::start) again.
    /// `Err(NoLease)` if there is no address to decline.
    pub fn decline(&mut self, _now_ns: u64) -> Result<Output, DhcpError> {
        let (addr, server) = match (self.offered, self.offered_server, &self.lease) {
            (Some(a), s, _) => (a, s.unwrap_or(crate::hostcfg::UNSPECIFIED)),
            (None, _, Some(l)) => (l.address, l.server_id),
            _ => return Err(DhcpError::NoLease),
        };
        self.lease = None;
        self.offered = None;
        self.offered_server = None;
        self.state = State::Init;
        self.t1_ns = 0;
        self.t2_ns = 0;
        self.expires_ns = 0;
        self.sent += 1;
        Ok(Output::Broadcast(build_decline(
            self.xid, self.mac, addr, server,
        )))
    }

    /// Give the lease back (RELEASE) and go idle. `Err(NoLease)` if not bound.
    pub fn release(&mut self) -> Result<Output, DhcpError> {
        let lease = self.lease.take().ok_or(DhcpError::NoLease)?;
        self.state = State::Init;
        self.t1_ns = 0;
        self.t2_ns = 0;
        self.expires_ns = 0;
        self.sent += 1;
        Ok(Output::Unicast {
            to: lease.server_id,
            data: build_release(self.xid, self.mac, lease.address, lease.server_id),
        })
    }
}

// ---- the hosted driver ----

/// How many DISCOVER/REQUEST attempts [`configure`] makes before giving up.
#[cfg(feature = "hosted")]
pub const CONFIGURE_ATTEMPTS: u32 = 3;

/// How long [`configure`] waits for a reply per attempt: **one second**, which is
/// generously above an emulated or on-link DHCP server's response time and is what
/// RFC 2131 §4.1's retransmission schedule starts from.
///
/// A **duration**, not a poll count: the wait parks in the kernel
/// ([`crate::wire::recv_frame_timeout`]) so it costs no CPU where an interrupt can
/// wake it, and one second means one second on every ISA.
#[cfg(feature = "hosted")]
pub const RECV_WINDOW_NS: u64 = 1_000_000_000;

/// How many datagrams [`configure`] will look at inside one window before
/// retransmitting - a frame count, so other traffic on the link cannot stretch the
/// attempt.
#[cfg(feature = "hosted")]
pub const RECV_FRAME_BUDGET: u32 = 32;

/// Send one client message over the NIC.
///
/// A [`Output::Broadcast`] is framed from `0.0.0.0` to `255.255.255.255` at the
/// Ethernet broadcast MAC with **no ARP**, which is the whole point (see the module
/// docs). A [`Output::Unicast`] resolves the server's MAC through the normal ARP
/// path, because by then we do have an address to ARP from.
#[cfg(feature = "hosted")]
pub async fn send(
    out: &Output,
    mac: crate::eth::Mac,
    src: Ipv4Addr,
    cache: &mut crate::arp::ArpCache,
) -> Result<(), crate::wire::WireError> {
    use crate::wire::{self, Ipv4Framing, WireError};

    let (dst_mac, src_ip, dst_ip) = match out {
        Output::Broadcast(_) => (
            crate::eth::BROADCAST,
            crate::hostcfg::UNSPECIFIED,
            crate::hostcfg::BROADCAST,
        ),
        Output::Unicast { to, .. } => (
            wire::resolve_next_hop(cache, mac, src, *to).await?,
            src,
            *to,
        ),
    };
    let payload = out.bytes();
    let mut datagram = [0u8; wire::MAX_FRAME - wire::L4_OFFSET];
    let dlen = crate::udp::build_v4(
        src_ip,
        dst_ip,
        CLIENT_PORT,
        SERVER_PORT,
        payload,
        &mut datagram,
    )
    .ok_or(WireError::TooBig)?;
    let framing = Ipv4Framing {
        dst_mac,
        src_mac: mac,
        ttl: wire::DEFAULT_TTL,
        protocol: crate::ip::proto::UDP,
        src_ip,
        dst_ip,
    };
    let mut frame = [0u8; wire::MAX_FRAME];
    let flen = wire::frame_ipv4(&framing, &datagram[..dlen], &mut frame)?;
    wire::send_frame(&frame[..flen]).await
}

/// Wait up to `window_ns` for one DHCP reply to `CLIENT_PORT`, copying the DHCP
/// message into `buf`.
///
/// Deliberately **not** built on [`crate::udp::UdpEndpoint::recv_from`]: an offer
/// arrives addressed to an address we do not own yet (or to the broadcast address),
/// so filtering on a local address would drop exactly the packet we are waiting
/// for. It filters on the destination **port** instead.
#[cfg(feature = "hosted")]
async fn recv_reply(buf: &mut [u8], window_ns: u64) -> Result<usize, DhcpError> {
    use crate::wire;

    let mut frame = [0u8; wire::MAX_FRAME];
    for _ in 0..RECV_FRAME_BUDGET {
        let n = wire::recv_frame_timeout(&mut frame, window_ns)
            .await
            .map_err(|_| DhcpError::Net)?;
        if n == 0 {
            break; // the window elapsed with no reply
        }
        let Some(parsed) = wire::parse_ipv4(&frame[..n]) else {
            continue;
        };
        if parsed.header.protocol != crate::ip::proto::UDP {
            continue;
        }
        let (start, end) = parsed.l4;
        let datagram = &frame[start..end];
        let Some(hdr) = crate::udp::UdpHeader::parse(datagram) else {
            continue;
        };
        if hdr.dst_port != CLIENT_PORT || hdr.src_port != SERVER_PORT {
            continue;
        }
        let Some(payload) = hdr.payload(datagram) else {
            continue;
        };
        let len = core::cmp::min(payload.len(), buf.len());
        buf[..len].copy_from_slice(&payload[..len]);
        return Ok(len);
    }
    Err(DhcpError::Timeout)
}

/// Run the DHCP exchange over the NIC until the client is BOUND, then write the
/// lease into `cfg`. The `hosted` driver over the [`Client`] state machine: it owns
/// only the transport and the retry loop; every decision is the state machine's.
///
/// Returns [`DhcpError::Timeout`] when no server answers - which is what happens
/// under QEMU's SLIRP, whose DHCP service answers the *host* stack, not the guest
/// on the emulated wire. The proof therefore treats a live lease as a bonus and the
/// state machine as the real assertion (docs/NETSTACK.md).
#[cfg(feature = "hosted")]
pub async fn configure(
    client: &mut Client,
    mac: crate::eth::Mac,
    cfg: &mut crate::hostcfg::HostConfig,
) -> Result<Lease, DhcpError> {
    let mut cache = crate::arp::ArpCache::new();
    let now = 0u64;
    let mut out = client.start(now);
    let mut buf = [0u8; 1024];
    for _ in 0..CONFIGURE_ATTEMPTS {
        send(&out, mac, cfg.source_address(), &mut cache)
            .await
            .map_err(|_| DhcpError::Net)?;
        let Ok(len) = recv_reply(&mut buf, RECV_WINDOW_NS).await else {
            // Nothing came back: retransmit the same message.
            continue;
        };
        match client.on_message(&buf[..len], now) {
            Ok((Event::Bound, _)) | Ok((Event::Renewed, _)) => {
                let lease = client.lease().expect("bound implies a lease").clone();
                cfg.apply_lease(&lease);
                return Ok(lease);
            }
            Ok((_, Some(next))) => out = next,
            // A message for someone else, or an unusable one: retransmit.
            Ok((_, None)) | Err(_) => {}
        }
    }
    Err(DhcpError::Timeout)
}
