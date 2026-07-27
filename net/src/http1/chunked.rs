//! `chunked` transfer-coding, both directions (RFC 9112 §7.1) - docs/NETSTACK.md
//! §19. Chunked framing is one of the two classic request-smuggling surfaces (the
//! other is `Content-Length`), so the decoder here is deliberately strict:
//!
//! - the chunk-size line must end in **CRLF**, never a bare LF;
//! - the size must be **1..=16 hex digits**, no sign, no leading whitespace, no
//!   `0x` prefix (`+5`, ` 5`, `0x5` are all rejected - these are the classic
//!   "one parser reads 5, the other reads 0" desync shapes);
//! - a chunk's data must be followed by exactly CRLF;
//! - the terminating `0` chunk must be followed by CRLF CRLF (an **empty**
//!   trailer section); a non-empty trailer is rejected, not silently dropped
//!   (dropping trailers is itself a smuggling vector through a proxy);
//! - the decoded body is capped at [`MAX_BODY`].
//!
//! Decoding is **one-shot over a complete buffer**: `decode` returns
//! [`Error::Incomplete`](super::Error::Incomplete) until the whole chunked body
//! has arrived, and the caller re-runs it as more bytes land. That re-scan is
//! O(n^2) in the number of feeds for one body and is the honest simplification
//! here (documented in docs/NETSTACK.md §19); a resumable decoder is a drop-in
//! replacement behind the same signature.

use alloc::vec::Vec;

use super::Error;

/// The largest decoded body the chunked decoder will produce (1 MiB). A bound is
/// mandatory: without it a chunked stream is an unbounded allocation.
pub const MAX_BODY: usize = 1024 * 1024;

/// Decode a complete chunked body from the front of `buf`.
///
/// Returns `(body, consumed)` where `consumed` counts every byte of the chunked
/// framing including the final CRLF CRLF. Returns [`Error::Incomplete`] if the
/// body has not fully arrived yet (the caller feeds more and retries) and a
/// specific error for every malformed shape.
pub fn decode(buf: &[u8]) -> Result<(Vec<u8>, usize), Error> {
    let mut out: Vec<u8> = Vec::new();
    let mut p = 0usize;
    loop {
        let (size, after_size) = read_chunk_size(buf, p)?;
        p = after_size;
        if size == 0 {
            // Last chunk: the trailer section must be empty, i.e. an immediate CRLF.
            if buf.len() < p + 2 {
                return Err(Error::Incomplete);
            }
            if &buf[p..p + 2] != b"\r\n" {
                // Anything else here is either a bare LF or a real trailer field.
                if buf[p] == b'\n' {
                    return Err(Error::BareLf);
                }
                return Err(Error::ChunkTrailerUnsupported);
            }
            return Ok((out, p + 2));
        }
        if out.len() + size > MAX_BODY {
            return Err(Error::BodyTooLarge);
        }
        if buf.len() < p + size + 2 {
            return Err(Error::Incomplete);
        }
        out.extend_from_slice(&buf[p..p + size]);
        p += size;
        if &buf[p..p + 2] != b"\r\n" {
            if buf[p] == b'\n' {
                return Err(Error::BareLf);
            }
            return Err(Error::BadChunkSize);
        }
        p += 2;
    }
}

/// Read one `chunk-size [ chunk-ext ] CRLF` line starting at `p`, returning
/// `(size, offset_just_past_the_CRLF)`.
fn read_chunk_size(buf: &[u8], p: usize) -> Result<(usize, usize), Error> {
    // Find the CRLF that ends the size line.
    let rest = &buf[p.min(buf.len())..];
    let lf = super::scan::find_byte(rest, b'\n');
    if lf == rest.len() {
        return Err(Error::Incomplete);
    }
    if lf == 0 || rest[lf - 1] != b'\r' {
        return Err(Error::BareLf);
    }
    let line = &rest[..lf - 1];
    // A chunk extension starts at the first ';' - accepted and ignored, but it
    // must not smuggle a CR/LF (it cannot: the line already ended at CRLF).
    let size_part = match super::scan::find_byte(line, b';') {
        i if i == line.len() => line,
        i => &line[..i],
    };
    if size_part.is_empty() || size_part.len() > 16 {
        return Err(Error::BadChunkSize);
    }
    let mut size = 0usize;
    for &b in size_part {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            // No sign, no whitespace, no `0x`: strictly hex digits only.
            _ => return Err(Error::BadChunkSize),
        };
        size = size * 16 + d as usize;
        if size > MAX_BODY {
            return Err(Error::BodyTooLarge);
        }
    }
    Ok((size, p + lf + 1))
}

/// Encode `body` as a chunked stream with chunks of at most `chunk_size` bytes,
/// terminated by the `0` chunk and an empty trailer section. `chunk_size` 0 is
/// treated as 1 (a zero-size chunk would be the terminator).
pub fn encode(body: &[u8], chunk_size: usize) -> Vec<u8> {
    let step = chunk_size.max(1);
    let mut out = Vec::with_capacity(body.len() + body.len() / step * 8 + 8);
    for c in body.chunks(step) {
        push_hex(&mut out, c.len());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(c);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// Append `v` as lowercase hex with no leading zeros.
fn push_hex(out: &mut Vec<u8>, v: usize) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut n = 0;
    let mut x = v;
    while x > 0 {
        buf[n] = D[x & 0xf];
        n += 1;
        x >>= 4;
    }
    for i in (0..n).rev() {
        out.push(buf[i]);
    }
}
