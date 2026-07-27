//! The HTTP/2 frame layer (RFC 9113 §4, §6) - docs/NETSTACK.md §19. Pure
//! encode/decode over byte slices: no allocation for the header, one `Vec` for a
//! built frame.
//!
//! Every frame is a 9-octet header - `length(24) | type(8) | flags(8) |
//! R(1) + stream_id(31)` - followed by `length` octets of payload. The reserved
//! bit is masked off on decode rather than rejected (RFC 9113 §4.1 says ignore).

use alloc::vec::Vec;

/// The fixed frame header length.
pub const HEADER_LEN: usize = 9;
/// `SETTINGS_MAX_FRAME_SIZE`'s default and minimum legal value (RFC 9113 §6.5.2).
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;
/// The initial flow-control window for every stream and the connection
/// (RFC 9113 §6.9.2).
pub const DEFAULT_INITIAL_WINDOW: u32 = 65_535;
/// The largest legal flow-control window (2^31 - 1).
pub const MAX_WINDOW: i64 = 0x7fff_ffff;

/// Frame types (RFC 9113 §6).
pub mod kind {
    pub const DATA: u8 = 0x0;
    pub const HEADERS: u8 = 0x1;
    pub const PRIORITY: u8 = 0x2;
    pub const RST_STREAM: u8 = 0x3;
    pub const SETTINGS: u8 = 0x4;
    pub const PUSH_PROMISE: u8 = 0x5;
    pub const PING: u8 = 0x6;
    pub const GOAWAY: u8 = 0x7;
    pub const WINDOW_UPDATE: u8 = 0x8;
    pub const CONTINUATION: u8 = 0x9;
}

/// Frame flags (RFC 9113 §6). `ACK` and `END_STREAM` share bit 0 on different
/// frame types, which is why they are separate names for the same value.
pub mod flag {
    pub const END_STREAM: u8 = 0x1;
    pub const ACK: u8 = 0x1;
    pub const END_HEADERS: u8 = 0x4;
    pub const PADDED: u8 = 0x8;
    pub const PRIORITY: u8 = 0x20;
}

/// Error codes (RFC 9113 §7).
pub mod err {
    pub const NO_ERROR: u32 = 0x0;
    pub const PROTOCOL_ERROR: u32 = 0x1;
    pub const INTERNAL_ERROR: u32 = 0x2;
    pub const FLOW_CONTROL_ERROR: u32 = 0x3;
    pub const SETTINGS_TIMEOUT: u32 = 0x4;
    pub const STREAM_CLOSED: u32 = 0x5;
    pub const FRAME_SIZE_ERROR: u32 = 0x6;
    pub const REFUSED_STREAM: u32 = 0x7;
    pub const CANCEL: u32 = 0x8;
    pub const COMPRESSION_ERROR: u32 = 0x9;
    pub const CONNECT_ERROR: u32 = 0xa;
    pub const ENHANCE_YOUR_CALM: u32 = 0xb;
    pub const INADEQUATE_SECURITY: u32 = 0xc;
    pub const HTTP_1_1_REQUIRED: u32 = 0xd;
}

/// Settings identifiers (RFC 9113 §6.5.2).
pub mod setting {
    pub const HEADER_TABLE_SIZE: u16 = 0x1;
    pub const ENABLE_PUSH: u16 = 0x2;
    pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    pub const INITIAL_WINDOW_SIZE: u16 = 0x4;
    pub const MAX_FRAME_SIZE: u16 = 0x5;
    pub const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// A decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub length: u32,
    pub kind: u8,
    pub flags: u8,
    pub stream_id: u32,
}

impl FrameHeader {
    /// Decode the 9-octet header. The reserved high bit of the stream id is
    /// masked off (RFC 9113 §4.1: a receiver must ignore it).
    pub fn decode(b: &[u8]) -> Option<FrameHeader> {
        if b.len() < HEADER_LEN {
            return None;
        }
        Some(FrameHeader {
            length: ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32,
            kind: b[3],
            flags: b[4],
            stream_id: u32::from_be_bytes([b[5], b[6], b[7], b[8]]) & 0x7fff_ffff,
        })
    }

    /// Encode the 9-octet header.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let sid = self.stream_id & 0x7fff_ffff;
        [
            (self.length >> 16) as u8,
            (self.length >> 8) as u8,
            self.length as u8,
            self.kind,
            self.flags,
            (sid >> 24) as u8,
            (sid >> 16) as u8,
            (sid >> 8) as u8,
            sid as u8,
        ]
    }
}

/// Build a complete frame (header + payload).
pub fn build(kind: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let h = FrameHeader {
        length: payload.len() as u32,
        kind,
        flags,
        stream_id,
    };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&h.encode());
    out.extend_from_slice(payload);
    out
}

/// Build a SETTINGS frame from `(id, value)` pairs.
pub fn build_settings(pairs: &[(u16, u32)]) -> Vec<u8> {
    let mut p = Vec::with_capacity(pairs.len() * 6);
    for (id, v) in pairs {
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&v.to_be_bytes());
    }
    build(kind::SETTINGS, 0, 0, &p)
}

/// Build an empty SETTINGS frame with the ACK flag.
pub fn build_settings_ack() -> Vec<u8> {
    build(kind::SETTINGS, flag::ACK, 0, &[])
}

/// Build a WINDOW_UPDATE frame.
pub fn build_window_update(stream_id: u32, increment: u32) -> Vec<u8> {
    build(
        kind::WINDOW_UPDATE,
        0,
        stream_id,
        &(increment & 0x7fff_ffff).to_be_bytes(),
    )
}

/// Build an RST_STREAM frame.
pub fn build_rst_stream(stream_id: u32, error_code: u32) -> Vec<u8> {
    build(kind::RST_STREAM, 0, stream_id, &error_code.to_be_bytes())
}

/// Build a GOAWAY frame.
pub fn build_goaway(last_stream_id: u32, error_code: u32, debug: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + debug.len());
    p.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
    p.extend_from_slice(&error_code.to_be_bytes());
    p.extend_from_slice(debug);
    build(kind::GOAWAY, 0, 0, &p)
}

/// Build a PING frame (`ack` selects a response to a peer's PING).
pub fn build_ping(payload: [u8; 8], ack: bool) -> Vec<u8> {
    build(kind::PING, if ack { flag::ACK } else { 0 }, 0, &payload)
}

/// Parse a SETTINGS payload into `(id, value)` pairs; `None` if its length is not
/// a multiple of 6 (a FRAME_SIZE_ERROR, RFC 9113 §6.5).
pub fn parse_settings(payload: &[u8]) -> Option<Vec<(u16, u32)>> {
    if !payload.len().is_multiple_of(6) {
        return None;
    }
    Some(
        payload
            .as_chunks::<6>()
            .0
            .iter()
            .map(|c| {
                (
                    u16::from_be_bytes([c[0], c[1]]),
                    u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
                )
            })
            .collect(),
    )
}

/// Strip the optional pad-length prefix and trailing padding from a DATA or
/// HEADERS payload, and the 5-byte priority block from a HEADERS payload.
/// Returns the header-block / data fragment, or `None` if the padding is
/// inconsistent (a PROTOCOL_ERROR, RFC 9113 §6.1).
pub fn strip_padding(payload: &[u8], flags: u8, has_priority_field: bool) -> Option<&[u8]> {
    let mut body = payload;
    let mut pad = 0usize;
    if flags & flag::PADDED != 0 {
        let (first, rest) = body.split_first()?;
        pad = *first as usize;
        body = rest;
    }
    if has_priority_field && flags & flag::PRIORITY != 0 {
        if body.len() < 5 {
            return None;
        }
        body = &body[5..];
    }
    if pad > body.len() {
        return None;
    }
    Some(&body[..body.len() - pad])
}
