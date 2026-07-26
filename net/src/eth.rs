//! Ethernet II: frame parse/build (docs/NETSTACK.md L2). A frame is
//! `[dst MAC (6)][src MAC (6)][ethertype (2, big-endian)][payload...]` - a
//! 14-byte header then the L3 payload. Parsing is **zero-copy**: [`Frame`] is a
//! view that borrows the underlying buffer and hands back slices; building writes
//! into a caller-provided buffer.
//!
//! The MAC type is `librheo::net::Mac`, re-exported so the whole stack (and the
//! raw-frame path underneath) share one address type.

pub use librheo::net::Mac;

/// The broadcast MAC (`ff:ff:ff:ff:ff:ff`).
pub const BROADCAST: Mac = Mac([0xff; 6]);

/// The Ethernet II header length in bytes.
pub const HEADER_LEN: usize = 14;

/// EtherType values (big-endian on the wire) for the protocols N1a touches.
pub mod ethertype {
    /// IPv4.
    pub const IPV4: u16 = 0x0800;
    /// ARP.
    pub const ARP: u16 = 0x0806;
    /// IPv6.
    pub const IPV6: u16 = 0x86DD;
}

/// A zero-copy view over an Ethernet II frame (borrows the backing buffer).
///
/// Constructed with [`Frame::parse`]; the accessors return the header fields and
/// a slice of the payload without copying.
#[derive(Copy, Clone)]
pub struct Frame<'a> {
    buf: &'a [u8],
}

impl<'a> Frame<'a> {
    /// View `buf` as an Ethernet frame, or `None` if it is shorter than the
    /// 14-byte header.
    pub fn parse(buf: &'a [u8]) -> Option<Frame<'a>> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Frame { buf })
    }

    /// Destination MAC.
    pub fn dst(&self) -> Mac {
        let mut m = [0u8; 6];
        m.copy_from_slice(&self.buf[0..6]);
        Mac(m)
    }

    /// Source MAC.
    pub fn src(&self) -> Mac {
        let mut m = [0u8; 6];
        m.copy_from_slice(&self.buf[6..12]);
        Mac(m)
    }

    /// EtherType (host order).
    pub fn ethertype(&self) -> u16 {
        u16::from_be_bytes([self.buf[12], self.buf[13]])
    }

    /// The payload after the 14-byte header (zero-copy slice of the buffer).
    pub fn payload(&self) -> &'a [u8] {
        &self.buf[HEADER_LEN..]
    }
}

/// The Ethernet II header, for building a frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
}

impl Header {
    /// Write the 14-byte header into `out[..14]`. Returns the bytes written, or
    /// `None` if `out` is too small.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < HEADER_LEN {
            return None;
        }
        out[0..6].copy_from_slice(&self.dst.0);
        out[6..12].copy_from_slice(&self.src.0);
        out[12..14].copy_from_slice(&self.ethertype.to_be_bytes());
        Some(HEADER_LEN)
    }
}

/// Build a complete frame (`header` + `payload`) into `out`, returning the total
/// length written, or `None` if `out` cannot hold `14 + payload.len()`.
pub fn build_frame(header: &Header, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return None;
    }
    header.write(out)?;
    out[HEADER_LEN..total].copy_from_slice(payload);
    Some(total)
}
