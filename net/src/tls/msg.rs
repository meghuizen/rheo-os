//! TLS 1.3 handshake message build/parse (RFC 8446 §4) - docs/NETSTACK.md §15.
//! Only the messages the 1-RTT handshake needs are here: ClientHello and
//! ServerHello with the `supported_versions` + `key_share` (X25519) extensions,
//! plus the generic handshake-header framing the later messages
//! (EncryptedExtensions / Certificate / CertificateVerify / Finished) reuse. The
//! ClientHello/ServerHello byte layout must match RFC 8448 exactly - the KAT
//! feeds the RFC's own ClientHello/ServerHello bytes through the transcript hash.
//!
//! Building is intentionally minimal (a single cipher-suite offer, one group);
//! the goal is a correct, provable handshake, not every extension. Parsing is
//! defensive (bounded reads, explicit errors) since these decode wire input.

use alloc::vec::Vec;

use super::TlsError;
use crate::crypto::hash::sha256;

/// TLS 1.3 handshake message types (RFC 8446 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeType {
    ClientHello,
    ServerHello,
    EncryptedExtensions,
    Certificate,
    CertificateVerify,
    Finished,
}

impl HandshakeType {
    pub fn to_u8(self) -> u8 {
        match self {
            HandshakeType::ClientHello => 1,
            HandshakeType::ServerHello => 2,
            HandshakeType::EncryptedExtensions => 8,
            HandshakeType::Certificate => 11,
            HandshakeType::CertificateVerify => 15,
            HandshakeType::Finished => 20,
        }
    }
    pub fn from_u8(v: u8) -> Option<HandshakeType> {
        match v {
            1 => Some(HandshakeType::ClientHello),
            2 => Some(HandshakeType::ServerHello),
            8 => Some(HandshakeType::EncryptedExtensions),
            11 => Some(HandshakeType::Certificate),
            15 => Some(HandshakeType::CertificateVerify),
            20 => Some(HandshakeType::Finished),
            _ => None,
        }
    }
}

/// The X25519 named group (RFC 8446 §4.2.7 / RFC 8422): `0x001d`.
pub const GROUP_X25519: u16 = 0x001d;
/// The Ed25519 signature scheme (RFC 8446 §4.2.3): `0x0807`.
pub const SIG_ED25519: u16 = 0x0807;

/// Wrap a handshake message body in the 4-byte handshake header
/// `msg_type(1) || length(3)` (RFC 8446 §4). The returned bytes are what goes
/// into the transcript hash.
pub fn handshake_message(kind: HandshakeType, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(4 + body.len());
    m.push(kind.to_u8());
    let l = body.len();
    m.push((l >> 16) as u8);
    m.push((l >> 8) as u8);
    m.push(l as u8);
    m.extend_from_slice(body);
    m
}

/// Split a handshake message into `(type, body)`, checking the 3-byte length.
pub fn parse_handshake(msg: &[u8]) -> Result<(HandshakeType, &[u8]), TlsError> {
    if msg.len() < 4 {
        return Err(TlsError::Decode);
    }
    let kind = HandshakeType::from_u8(msg[0]).ok_or(TlsError::Decode)?;
    let len = ((msg[1] as usize) << 16) | ((msg[2] as usize) << 8) | msg[3] as usize;
    if 4 + len != msg.len() {
        return Err(TlsError::Decode);
    }
    Ok((kind, &msg[4..]))
}

/// The running handshake transcript (RFC 8446 §4.4.1): the concatenation of the
/// handshake messages, hashed on demand. Kept as the raw bytes so any prefix hash
/// (CH..Cert for CertificateVerify, CH..CertVerify for the server Finished) is a
/// single SHA-256 - clearer than snapshotting an incremental hasher, and this is
/// not a hot path.
#[derive(Default)]
pub struct Transcript {
    bytes: Vec<u8>,
}

impl Transcript {
    pub fn new() -> Transcript {
        Transcript { bytes: Vec::new() }
    }
    /// Append one handshake message (already header-framed).
    pub fn add(&mut self, msg: &[u8]) {
        self.bytes.extend_from_slice(msg);
    }
    /// The SHA-256 transcript hash over everything added so far.
    pub fn hash(&self) -> [u8; 32] {
        sha256(&self.bytes)
    }
}

/// Build a ClientHello body (RFC 8446 §4.1.2) offering `cipher_suites`, X25519 as
/// the sole group with `key_share_pub` as our share, TLS 1.3 in
/// `supported_versions`, and Ed25519 in `signature_algorithms`. `random` and
/// `session_id` are caller-supplied so a KAT can reproduce fixed bytes.
pub fn build_client_hello(
    random: &[u8; 32],
    session_id: &[u8],
    cipher_suites: &[u16],
    key_share_pub: &[u8; 32],
) -> Vec<u8> {
    build_client_hello_alpn(random, session_id, cipher_suites, key_share_pub, &[])
}

/// The ALPN extension type (RFC 7301 §3.1): `application_layer_protocol_negotiation`.
pub const EXT_ALPN: u16 = 0x0010;

/// Encode an ALPN `ProtocolNameList`: a 2-byte total length, then each protocol as
/// a 1-byte length + its bytes (RFC 7301 §3.1).
pub fn build_alpn_list(protocols: &[&[u8]]) -> Vec<u8> {
    let mut inner = Vec::new();
    for p in protocols {
        inner.push(p.len() as u8);
        inner.extend_from_slice(p);
    }
    let mut out = Vec::with_capacity(2 + inner.len());
    out.extend_from_slice(&(inner.len() as u16).to_be_bytes());
    out.extend_from_slice(&inner);
    out
}

/// Parse an ALPN `ProtocolNameList` into its protocol names. Bounded reads; a
/// malformed list yields an empty vector rather than an error, which is what a
/// server that simply declines to negotiate needs.
pub fn parse_alpn_list(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.len() < 2 {
        return out;
    }
    let total = u16::from_be_bytes([data[0], data[1]]) as usize;
    let body = &data[2..];
    if total > body.len() {
        return out;
    }
    let mut i = 0;
    while i < total {
        let l = body[i] as usize;
        if i + 1 + l > total {
            break;
        }
        out.push(body[i + 1..i + 1 + l].to_vec());
        i += 1 + l;
    }
    out
}

/// Build a ClientHello that also offers ALPN (RFC 7301) when `alpn` is non-empty.
/// With an empty list this is byte-for-byte [`build_client_hello`], so the RFC 8448
/// known-answer test and the existing handshake are unaffected.
pub fn build_client_hello_alpn(
    random: &[u8; 32],
    session_id: &[u8],
    cipher_suites: &[u16],
    key_share_pub: &[u8; 32],
    alpn: &[&[u8]],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0x03, 0x03]); // legacy_version
    b.extend_from_slice(random);
    b.push(session_id.len() as u8);
    b.extend_from_slice(session_id);
    // cipher_suites<2..2^16-2>
    b.extend_from_slice(&((cipher_suites.len() * 2) as u16).to_be_bytes());
    for &cs in cipher_suites {
        b.extend_from_slice(&cs.to_be_bytes());
    }
    b.extend_from_slice(&[0x01, 0x00]); // legacy_compression_methods = {0}

    // Extensions.
    let mut ext = Vec::new();
    // supported_versions (0x002b): list of one, TLS 1.3.
    push_ext(&mut ext, 0x002b, &[0x02, 0x03, 0x04]);
    // supported_groups (0x000a): x25519.
    push_ext(&mut ext, 0x000a, &[0x00, 0x02, 0x00, 0x1d]);
    // signature_algorithms (0x000d): ed25519.
    push_ext(&mut ext, 0x000d, &[0x00, 0x02, 0x08, 0x07]);
    // key_share (0x0033): one KeyShareEntry { x25519, 32-byte key }.
    let mut ks = Vec::new();
    ks.extend_from_slice(&GROUP_X25519.to_be_bytes());
    ks.extend_from_slice(&(32u16).to_be_bytes());
    ks.extend_from_slice(key_share_pub);
    let mut ks_list = Vec::new();
    ks_list.extend_from_slice(&(ks.len() as u16).to_be_bytes());
    ks_list.extend_from_slice(&ks);
    push_ext(&mut ext, 0x0033, &ks_list);
    // ALPN (0x0010), only when the caller offers protocols - so an empty list
    // reproduces the original ClientHello bytes exactly.
    if !alpn.is_empty() {
        push_ext(&mut ext, EXT_ALPN, &build_alpn_list(alpn));
    }

    b.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    b.extend_from_slice(&ext);
    b
}

/// Build a ServerHello body (RFC 8446 §4.1.3) selecting `cipher_suite`, echoing
/// `session_id`, with `supported_versions` (TLS 1.3) and a `key_share` carrying
/// the server's X25519 share.
pub fn build_server_hello(
    random: &[u8; 32],
    session_id: &[u8],
    cipher_suite: u16,
    key_share_pub: &[u8; 32],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0x03, 0x03]);
    b.extend_from_slice(random);
    b.push(session_id.len() as u8);
    b.extend_from_slice(session_id);
    b.extend_from_slice(&cipher_suite.to_be_bytes());
    b.push(0x00); // legacy_compression_method

    let mut ext = Vec::new();
    push_ext(&mut ext, 0x002b, &[0x03, 0x04]); // supported_versions (selected)
    let mut ks = Vec::new();
    ks.extend_from_slice(&GROUP_X25519.to_be_bytes());
    ks.extend_from_slice(&(32u16).to_be_bytes());
    ks.extend_from_slice(key_share_pub);
    push_ext(&mut ext, 0x0033, &ks); // key_share (server: a single entry)

    b.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    b.extend_from_slice(&ext);
    b
}

/// Append an extension `ext_type(2) || length(2) || data` to `out`.
fn push_ext(out: &mut Vec<u8>, ext_type: u16, data: &[u8]) {
    out.extend_from_slice(&ext_type.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// The cipher suites a parsed ClientHello offered, in order.
pub struct ClientHelloInfo {
    pub cipher_suites: Vec<u16>,
    pub key_share_x25519: Option<[u8; 32]>,
    pub session_id: Vec<u8>,
    /// The ALPN protocol names offered (RFC 7301), empty if the extension was
    /// absent. A server picks the first of these it supports.
    pub alpn: Vec<Vec<u8>>,
}

/// Parse a ClientHello body far enough to pick a suite and read the peer's X25519
/// key share (the fields a server needs). Defensive, bounded reads.
pub fn parse_client_hello(body: &[u8]) -> Result<ClientHelloInfo, TlsError> {
    let mut c = Cursor::new(body);
    c.skip(2)?; // legacy_version
    c.skip(32)?; // random
    let sid_len = c.u8()? as usize;
    let session_id = c.take(sid_len)?.to_vec();
    let cs_len = c.u16()? as usize;
    let cs_bytes = c.take(cs_len)?;
    let mut cipher_suites = Vec::new();
    let mut i = 0;
    while i + 1 < cs_bytes.len() {
        cipher_suites.push(u16::from_be_bytes([cs_bytes[i], cs_bytes[i + 1]]));
        i += 2;
    }
    let comp_len = c.u8()? as usize;
    c.skip(comp_len)?;
    let (key_share_x25519, alpn) = parse_extensions(&mut c, true)?;
    Ok(ClientHelloInfo {
        cipher_suites,
        key_share_x25519,
        session_id,
        alpn,
    })
}

/// Read the server's selected ALPN protocol out of an EncryptedExtensions body
/// (RFC 7301 §3.2: a single-entry `ProtocolNameList`). `None` if the server did
/// not negotiate one.
pub fn parse_encrypted_extensions_alpn(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < 2 {
        return None;
    }
    let total = u16::from_be_bytes([body[0], body[1]]) as usize;
    let ext = &body[2..];
    if total > ext.len() {
        return None;
    }
    let mut i = 0;
    while i + 4 <= total {
        let ty = u16::from_be_bytes([ext[i], ext[i + 1]]);
        let len = u16::from_be_bytes([ext[i + 2], ext[i + 3]]) as usize;
        if i + 4 + len > total {
            return None;
        }
        if ty == EXT_ALPN {
            return parse_alpn_list(&ext[i + 4..i + 4 + len]).into_iter().next();
        }
        i += 4 + len;
    }
    None
}

/// Parse a ServerHello body: return `(selected_suite, server_x25519_share)`.
pub fn parse_server_hello(body: &[u8]) -> Result<(u16, [u8; 32]), TlsError> {
    let mut c = Cursor::new(body);
    c.skip(2)?; // legacy_version
    c.skip(32)?; // random
    let sid_len = c.u8()? as usize;
    c.skip(sid_len)?;
    let suite = c.u16()?;
    c.skip(1)?; // legacy_compression_method
    let (share, _alpn) = parse_extensions(&mut c, false)?;
    Ok((suite, share.ok_or(TlsError::MissingKeyShare)?))
}

/// What the extension walk extracts: the peer's X25519 key share (if any) and the
/// ALPN protocol list (empty when the extension is absent).
type ExtensionInfo = (Option<[u8; 32]>, Vec<Vec<u8>>);

/// Walk the extensions block, returning the X25519 key share and any ALPN
/// protocol list. For a ClientHello the key_share is a list of entries; for a
/// ServerHello it is a single entry.
fn parse_extensions(c: &mut Cursor, is_client_hello: bool) -> Result<ExtensionInfo, TlsError> {
    let ext_total = c.u16()? as usize;
    let ext_bytes = c.take(ext_total)?;
    let mut e = Cursor::new(ext_bytes);
    let mut found = None;
    let mut alpn = Vec::new();
    while e.remaining() >= 4 {
        let ext_type = e.u16()?;
        let ext_len = e.u16()? as usize;
        let data = e.take(ext_len)?;
        if ext_type == 0x0033 {
            found = parse_key_share_body(data, is_client_hello);
        } else if ext_type == EXT_ALPN {
            alpn = parse_alpn_list(data);
        }
    }
    Ok((found, alpn))
}

fn parse_key_share_body(data: &[u8], is_client_hello: bool) -> Option<[u8; 32]> {
    let entries = if is_client_hello {
        if data.len() < 2 {
            return None;
        }
        &data[2..] // skip client_shares length
    } else {
        data
    };
    let mut i = 0;
    while i + 4 <= entries.len() {
        let group = u16::from_be_bytes([entries[i], entries[i + 1]]);
        let klen = u16::from_be_bytes([entries[i + 2], entries[i + 3]]) as usize;
        let kstart = i + 4;
        if kstart + klen > entries.len() {
            return None;
        }
        if group == GROUP_X25519 && klen == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&entries[kstart..kstart + 32]);
            return Some(k);
        }
        i = kstart + klen;
    }
    None
}

/// A tiny bounds-checked byte cursor for the parsers.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], TlsError> {
        if self.pos + n > self.buf.len() {
            return Err(TlsError::Decode);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<(), TlsError> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8, TlsError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, TlsError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}
