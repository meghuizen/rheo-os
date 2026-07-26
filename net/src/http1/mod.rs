//! `net::http1` - HTTP/1.1 (RFC 9110 semantics, RFC 9112 framing), rheo-net Phase
//! N5a (docs/NETSTACK.md §19). Pure codec + byte-stream drivers: **no kernel
//! object, no per-ISA code, no dependency**. It compiles in **both** rheo-net
//! postures (the librheo-hosted cell build and the librheo-free codec build the
//! kernel links), because nothing here touches the NIC, the clock or the heap
//! beyond `alloc`.
//!
//! ## Zero-copy: the parse layer borrows, the convenience layer owns
//! [`parse_request`] / [`parse_response`] return views whose method, target,
//! reason phrase and every header name/value are `&[u8]` **slices of the caller's
//! buffer** - the same discipline as rheo-json's borrowed `Cow`. Nothing is
//! copied, nothing is lowercased in place; case-insensitive lookup happens at
//! compare time ([`scan::eq_ignore_case`]). That is what a WAF / DPI datapath
//! needs: inspect a request without allocating per header. The [`Client`] /
//! [`Server`] helpers on top hand back owned [`OwnedResponse`] /
//! [`OwnedRequest`] values, because a response outlives the socket buffer it
//! arrived in - the copy is at that boundary and nowhere else.
//!
//! ## Request smuggling is a parser property, not a filter
//! Every desync shape below is rejected **in the parser**, with its own error, so
//! no caller can accidentally accept one (RFC 9112 §6.1, §6.3):
//!
//! | shape | error |
//! |---|---|
//! | `Content-Length` **and** `Transfer-Encoding` on one message | [`Error::BothLengthAndEncoding`] |
//! | two `Content-Length` fields (equal or not) | [`Error::DuplicateContentLength`] |
//! | `Content-Length: 5, 5` / non-digit / signed | [`Error::BadContentLength`] |
//! | `Transfer-Encoding` whose final coding is not `chunked` | [`Error::BadTransferEncoding`] |
//! | a bare LF where CRLF is required | [`Error::BareLf`] |
//! | `Header : value` (whitespace before the colon) | [`Error::SpaceBeforeColon`] |
//! | an obs-fold continuation line | [`Error::ObsFold`] |
//! | a non-token byte in a header name | [`Error::BadHeaderName`] |
//! | a header block over [`MAX_HEADER_BYTES`] or over [`MAX_HEADERS`] fields | [`Error::HeaderBlockTooLarge`] / [`Error::TooManyHeaders`] |
//!
//! The chunked decoder is equally strict - see [`chunked`].
//!
//! ## What N5a's HTTP/1.1 does not do (honest)
//! No `Upgrade` / `CONNECT` tunnelling, no trailers (rejected, not ignored), no
//! `100-continue`, no content codings (`gzip`), no multipart, no cookie parsing,
//! no pipelining depth beyond "one request in flight per connection" (keep-alive
//! reuse **is** proven), and the response body without `Content-Length` or
//! `Transfer-Encoding` is close-delimited ([`Body::Eof`]) rather than streamed.

pub mod chunked;
pub mod scan;

use alloc::vec::Vec;

/// The largest header block (start line + all fields + the terminating CRLF) the
/// parser will accept, in bytes. A bound is not optional: an unbounded header
/// block is a trivial memory-exhaustion vector.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// The largest number of header fields accepted in one message.
pub const MAX_HEADERS: usize = 64;

/// Every way an HTTP/1.1 message can be rejected. [`Error::Incomplete`] is the
/// only non-fatal one: it means "feed me more bytes". Everything else is a
/// protocol violation the caller must answer with 400 and close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The buffer does not yet hold a complete message - not an error, retry.
    Incomplete,
    /// The request line is not `method SP request-target SP HTTP-version`.
    BadRequestLine,
    /// The status line is not `HTTP-version SP status-code [SP reason]`.
    BadStatusLine,
    /// The version token is not `HTTP/1.0` or `HTTP/1.1`.
    BadVersion,
    /// A header name is empty or contains a non-token byte.
    BadHeaderName,
    /// A header value contains a control byte (CR/LF/NUL injection).
    BadHeaderValue,
    /// Whitespace between a header name and its colon (`Host : x`) - RFC 9112
    /// §5.1 requires this be rejected, it is a proxy-desync vector.
    SpaceBeforeColon,
    /// An obs-fold (a field line starting with SP/HTAB) - RFC 9112 §5.2.
    ObsFold,
    /// A bare LF where the grammar requires CRLF.
    BareLf,
    /// The header block exceeded [`MAX_HEADER_BYTES`].
    HeaderBlockTooLarge,
    /// The message carried more than [`MAX_HEADERS`] header fields.
    TooManyHeaders,
    /// Both `Content-Length` and `Transfer-Encoding` are present (smuggling).
    BothLengthAndEncoding,
    /// More than one `Content-Length` field (smuggling).
    DuplicateContentLength,
    /// A `Content-Length` that is not a plain non-negative decimal integer.
    BadContentLength,
    /// A `Transfer-Encoding` whose final coding is not `chunked`.
    BadTransferEncoding,
    /// A malformed chunk-size line.
    BadChunkSize,
    /// A chunked message with a non-empty trailer section (deferred, rejected).
    ChunkTrailerUnsupported,
    /// A body larger than the decoder's cap ([`chunked::MAX_BODY`]).
    BodyTooLarge,
}

/// The HTTP/1.x minor version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

impl Version {
    /// The on-wire token.
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            Version::Http10 => b"HTTP/1.0",
            Version::Http11 => b"HTTP/1.1",
        }
    }
    fn parse(t: &[u8]) -> Result<Version, Error> {
        match t {
            b"HTTP/1.1" => Ok(Version::Http11),
            b"HTTP/1.0" => Ok(Version::Http10),
            _ => Err(Error::BadVersion),
        }
    }
}

/// One header field, **borrowed** from the parsed buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

/// The header fields of one message, in wire order, all borrowed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Headers<'a> {
    fields: Vec<Header<'a>>,
}

impl<'a> Headers<'a> {
    /// The fields in wire order.
    pub fn as_slice(&self) -> &[Header<'a>] {
        &self.fields
    }
    /// Number of fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
    /// The **first** value for `name` (ASCII case-insensitive), borrowed.
    pub fn get(&self, name: &[u8]) -> Option<&'a [u8]> {
        self.fields
            .iter()
            .find(|h| scan::eq_ignore_case(h.name, name))
            .map(|h| h.value)
    }
    /// How many fields carry `name` (case-insensitive) - the duplicate check the
    /// smuggling defences rest on.
    pub fn count(&self, name: &[u8]) -> usize {
        self.fields
            .iter()
            .filter(|h| scan::eq_ignore_case(h.name, name))
            .count()
    }
    /// Every value for `name`, in wire order.
    pub fn get_all<'b>(&'b self, name: &'b [u8]) -> impl Iterator<Item = &'a [u8]> + 'b {
        self.fields
            .iter()
            .filter(move |h| scan::eq_ignore_case(h.name, name))
            .map(|h| h.value)
    }
    /// True if `name` has a value whose comma-separated list contains `token`
    /// (case-insensitive) - the `Connection: keep-alive, foo` shape.
    pub fn has_token(&self, name: &[u8], token: &[u8]) -> bool {
        self.get_all(name).any(|v| {
            v.split(|&b| b == b',')
                .any(|t| scan::eq_ignore_case(scan::trim_ows(t), token))
        })
    }
}

/// A parsed request - every field borrows the input buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: &'a [u8],
    pub target: &'a [u8],
    pub version: Version,
    pub headers: Headers<'a>,
    /// Bytes of the header block, i.e. where the body starts in the input.
    pub header_len: usize,
}

/// A parsed response - every field borrows the input buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<'a> {
    pub version: Version,
    pub status: u16,
    pub reason: &'a [u8],
    pub headers: Headers<'a>,
    /// Bytes of the header block, i.e. where the body starts in the input.
    pub header_len: usize,
}

/// How a message's body is delimited (RFC 9112 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// No body at all.
    None,
    /// Exactly `n` bytes follow the header block.
    Length(usize),
    /// `Transfer-Encoding: chunked` framing follows.
    Chunked,
    /// The body runs to connection close (responses only).
    Eof,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Split the header block at the front of `buf` into its lines.
///
/// Enforces, in one place, the line-level smuggling defences: **every** LF must
/// be preceded by CR (no bare LF anywhere in the block), no field line may begin
/// with SP/HTAB (no obs-fold), and the block is bounded in both bytes and field
/// count. Returns `(lines, header_len)`.
fn split_block(buf: &[u8]) -> Result<(Vec<&[u8]>, usize), Error> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut pos = 0usize;
    loop {
        let rest = &buf[pos..];
        let lf = scan::find_byte(rest, b'\n');
        if lf == rest.len() {
            // No LF yet. Bound the wait so a peer cannot stream forever.
            if buf.len() > MAX_HEADER_BYTES {
                return Err(Error::HeaderBlockTooLarge);
            }
            return Err(Error::Incomplete);
        }
        if lf == 0 || rest[lf - 1] != b'\r' {
            return Err(Error::BareLf);
        }
        let line = &rest[..lf - 1];
        pos += lf + 1;
        if pos > MAX_HEADER_BYTES {
            return Err(Error::HeaderBlockTooLarge);
        }
        if line.is_empty() {
            // The empty line ends the block.
            return Ok((lines, pos));
        }
        // An obs-fold continuation line must be rejected, not unfolded: a proxy
        // that unfolds and one that rejects disagree on where the body starts.
        if !lines.is_empty() && matches!(line.first(), Some(b' ') | Some(b'\t')) {
            return Err(Error::ObsFold);
        }
        if lines.len() > MAX_HEADERS {
            return Err(Error::TooManyHeaders);
        }
        lines.push(line);
    }
}

/// Parse one header field line into a borrowed [`Header`], validating the name as
/// a token and the value as field-vchars.
fn parse_field(line: &[u8]) -> Result<Header<'_>, Error> {
    let colon = scan::find_byte(line, b':');
    if colon == line.len() {
        return Err(Error::BadHeaderName);
    }
    let name = &line[..colon];
    if name.is_empty() {
        return Err(Error::BadHeaderName);
    }
    // `Host : x` - the whitespace-before-colon desync. Detect it specifically so
    // the error names the actual attack rather than "bad name".
    if matches!(name.last(), Some(b' ') | Some(b'\t')) {
        return Err(Error::SpaceBeforeColon);
    }
    if scan::token_end(name) != name.len() {
        return Err(Error::BadHeaderName);
    }
    let raw = &line[colon + 1..];
    if !raw.iter().all(|&b| scan::is_field_vchar(b)) {
        return Err(Error::BadHeaderValue);
    }
    Ok(Header {
        name,
        value: scan::trim_ows(raw),
    })
}

/// Split a start line on single spaces into at most `n` parts, rejecting empty
/// parts and any run of spaces (`GET  / HTTP/1.1` is a desync shape).
fn split_start_line(line: &[u8], n: usize) -> Option<Vec<&[u8]>> {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(n);
    let mut start = 0usize;
    for i in 0..=line.len() {
        if i == line.len() || line[i] == b' ' {
            if parts.len() + 1 > n {
                return None;
            }
            let part = &line[start..i];
            if part.is_empty() {
                return None;
            }
            parts.push(part);
            start = i + 1;
            if i == line.len() {
                break;
            }
        }
    }
    Some(parts)
}

/// Parse a request from the front of `buf`. Every returned slice borrows `buf`.
pub fn parse_request(buf: &[u8]) -> Result<Request<'_>, Error> {
    let (lines, header_len) = split_block(buf)?;
    let first = lines.first().copied().ok_or(Error::BadRequestLine)?;
    let parts = split_start_line(first, 3).ok_or(Error::BadRequestLine)?;
    if parts.len() != 3 {
        return Err(Error::BadRequestLine);
    }
    let method = parts[0];
    if scan::token_end(method) != method.len() {
        return Err(Error::BadRequestLine);
    }
    let target = parts[1];
    // A request target must be visible ASCII: no controls, no space (already
    // excluded by the split), no 8-bit bytes.
    if !target.iter().all(|&b| (0x21..=0x7e).contains(&b)) {
        return Err(Error::BadRequestLine);
    }
    let version = Version::parse(parts[2])?;

    let mut headers = Headers::default();
    for line in &lines[1..] {
        headers.fields.push(parse_field(line)?);
    }
    Ok(Request {
        method,
        target,
        version,
        headers,
        header_len,
    })
}

/// Parse a response from the front of `buf`. Every returned slice borrows `buf`.
pub fn parse_response(buf: &[u8]) -> Result<Response<'_>, Error> {
    let (lines, header_len) = split_block(buf)?;
    let first = lines.first().copied().ok_or(Error::BadStatusLine)?;
    // `HTTP/1.1 200 OK` - the reason phrase may itself contain spaces, so split
    // only the first two fields off by hand.
    let sp1 = scan::find_byte(first, b' ');
    if sp1 == first.len() {
        return Err(Error::BadStatusLine);
    }
    let version = Version::parse(&first[..sp1])?;
    let rest = &first[sp1 + 1..];
    if rest.len() < 3 || !rest[..3].iter().all(|b| b.is_ascii_digit()) {
        return Err(Error::BadStatusLine);
    }
    let status =
        (rest[0] - b'0') as u16 * 100 + (rest[1] - b'0') as u16 * 10 + (rest[2] - b'0') as u16;
    let reason = match rest.len() {
        3 => &rest[3..],
        _ if rest[3] == b' ' => &rest[4..],
        _ => return Err(Error::BadStatusLine),
    };
    if !reason.iter().all(|&b| scan::is_field_vchar(b)) {
        return Err(Error::BadStatusLine);
    }

    let mut headers = Headers::default();
    for line in &lines[1..] {
        headers.fields.push(parse_field(line)?);
    }
    Ok(Response {
        version,
        status,
        reason,
        headers,
        header_len,
    })
}

/// The shared `Content-Length` / `Transfer-Encoding` framing decision, with all
/// the smuggling rejections. `default_eof` distinguishes a response (a body with
/// no framing runs to close) from a request (no framing means no body).
fn body_from_headers(h: &Headers<'_>, default_eof: bool) -> Result<Body, Error> {
    let te = h.count(b"transfer-encoding");
    let cl = h.count(b"content-length");
    // The headline smuggling shape: a front end that honours Content-Length and a
    // back end that honours Transfer-Encoding disagree on the message boundary.
    if te > 0 && cl > 0 {
        return Err(Error::BothLengthAndEncoding);
    }
    if te > 0 {
        if te > 1 {
            return Err(Error::BadTransferEncoding);
        }
        let v = h.get(b"transfer-encoding").unwrap_or(b"");
        // The **final** coding must be chunked, and this slice supports no other
        // coding at all, so require exactly `chunked`.
        if !scan::eq_ignore_case(scan::trim_ows(v), b"chunked") {
            return Err(Error::BadTransferEncoding);
        }
        return Ok(Body::Chunked);
    }
    if cl > 1 {
        // Rejected whether or not the values agree: RFC 9112 §6.3 lets a
        // recipient reject, and a proxy that collapses duplicates while its peer
        // takes the first is exactly the desync.
        return Err(Error::DuplicateContentLength);
    }
    if cl == 1 {
        let v = h.get(b"content-length").unwrap_or(b"");
        return Ok(Body::Length(parse_content_length(v)?));
    }
    Ok(if default_eof { Body::Eof } else { Body::None })
}

/// Parse a `Content-Length` value: 1..=19 ASCII digits and nothing else. No
/// sign, no whitespace inside, no comma list (`5, 5`), no hex.
fn parse_content_length(v: &[u8]) -> Result<usize, Error> {
    if v.is_empty() || v.len() > 19 || !v.iter().all(|b| b.is_ascii_digit()) {
        return Err(Error::BadContentLength);
    }
    let mut n: usize = 0;
    for &b in v {
        n = n
            .checked_mul(10)
            .and_then(|x| x.checked_add((b - b'0') as usize))
            .ok_or(Error::BadContentLength)?;
    }
    Ok(n)
}

impl Request<'_> {
    /// How this request's body is delimited, with every smuggling shape rejected.
    pub fn body(&self) -> Result<Body, Error> {
        body_from_headers(&self.headers, false)
    }
    /// Whether the connection should be reused after this request (RFC 9112 §9.3):
    /// HTTP/1.1 persists unless `Connection: close`; HTTP/1.0 does not unless
    /// `Connection: keep-alive`.
    pub fn keep_alive(&self) -> bool {
        keep_alive(self.version, &self.headers)
    }
}

impl Response<'_> {
    /// How this response's body is delimited. `head_request` must be true if this
    /// answers a HEAD (which has no body whatever the headers say).
    pub fn body(&self, head_request: bool) -> Result<Body, Error> {
        if head_request
            || self.status == 204
            || self.status == 304
            || (100..200).contains(&self.status)
        {
            // Still validate the framing headers, so a smuggling attempt on a
            // bodiless response is rejected rather than ignored.
            body_from_headers(&self.headers, false)?;
            return Ok(Body::None);
        }
        body_from_headers(&self.headers, true)
    }
    /// Whether the connection should be reused after this response.
    pub fn keep_alive(&self) -> bool {
        keep_alive(self.version, &self.headers)
    }
}

/// The persistence rule for a version + header set (RFC 9112 §9.3).
pub fn keep_alive(version: Version, headers: &Headers<'_>) -> bool {
    if headers.has_token(b"connection", b"close") {
        return false;
    }
    match version {
        Version::Http11 => true,
        Version::Http10 => headers.has_token(b"connection", b"keep-alive"),
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn push_headers(out: &mut Vec<u8>, headers: &[(&[u8], &[u8])]) {
    for (n, v) in headers {
        out.extend_from_slice(n);
        out.extend_from_slice(b": ");
        out.extend_from_slice(v);
        out.extend_from_slice(b"\r\n");
    }
}

fn push_decimal(out: &mut Vec<u8>, v: usize) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    while x > 0 {
        buf[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
    }
    for i in (0..n).rev() {
        out.push(buf[i]);
    }
}

/// Serialise a request. When `body` is `Some`, a `Content-Length` is emitted
/// automatically (the caller must not also pass one).
pub fn write_request(
    method: &[u8],
    target: &[u8],
    version: Version,
    headers: &[(&[u8], &[u8])],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + target.len() + body.map_or(0, |b| b.len()));
    out.extend_from_slice(method);
    out.push(b' ');
    out.extend_from_slice(target);
    out.push(b' ');
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\r\n");
    push_headers(&mut out, headers);
    if let Some(b) = body {
        out.extend_from_slice(b"content-length: ");
        push_decimal(&mut out, b.len());
        out.extend_from_slice(b"\r\n\r\n");
        out.extend_from_slice(b);
    } else {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Serialise a response with a `Content-Length`-delimited body.
pub fn write_response(
    status: u16,
    reason: &[u8],
    version: Version,
    headers: &[(&[u8], &[u8])],
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + body.len());
    out.extend_from_slice(version.as_bytes());
    out.push(b' ');
    push_decimal(&mut out, status as usize);
    if !reason.is_empty() {
        out.push(b' ');
        out.extend_from_slice(reason);
    }
    out.extend_from_slice(b"\r\n");
    push_headers(&mut out, headers);
    out.extend_from_slice(b"content-length: ");
    push_decimal(&mut out, body.len());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(body);
    out
}

/// Serialise a response with a **chunked** body of `chunk_size`-byte chunks.
pub fn write_response_chunked(
    status: u16,
    reason: &[u8],
    headers: &[(&[u8], &[u8])],
    body: &[u8],
    chunk_size: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + body.len() + 32);
    out.extend_from_slice(Version::Http11.as_bytes());
    out.push(b' ');
    push_decimal(&mut out, status as usize);
    if !reason.is_empty() {
        out.push(b' ');
        out.extend_from_slice(reason);
    }
    out.extend_from_slice(b"\r\n");
    push_headers(&mut out, headers);
    out.extend_from_slice(b"transfer-encoding: chunked\r\n\r\n");
    out.extend_from_slice(&chunked::encode(body, chunk_size));
    out
}

// ---------------------------------------------------------------------------
// Byte-stream client / server
// ---------------------------------------------------------------------------

/// An owned response: what a [`Client`] hands back once a whole message has
/// arrived. Owned because it outlives the receive buffer (see the module doc on
/// where the one copy lives).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedResponse {
    pub status: u16,
    pub reason: Vec<u8>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl OwnedResponse {
    /// The first value for `name`, case-insensitive.
    pub fn header(&self, name: &[u8]) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| scan::eq_ignore_case(n, name))
            .map(|(_, v)| v.as_slice())
    }
}

/// An owned request: what a [`Server`] hands to its router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRequest {
    pub method: Vec<u8>,
    pub target: Vec<u8>,
    pub version: Version,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl OwnedRequest {
    /// The first value for `name`, case-insensitive.
    pub fn header(&self, name: &[u8]) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| scan::eq_ignore_case(n, name))
            .map(|(_, v)| v.as_slice())
    }
}

fn own_headers(h: &Headers<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
    h.as_slice()
        .iter()
        .map(|f| (f.name.to_vec(), f.value.to_vec()))
        .collect()
}

/// An HTTP/1.1 **client** over any byte transport. It is transport-agnostic on
/// purpose: it produces request bytes and consumes response bytes, so the same
/// client drives a [`crate::tcp::TcpStream`] (via the synchronous
/// `poll`/`on_wire_segment` seam), a TLS record layer (HTTPS - the bytes are just
/// the record plaintext), or an in-cell loopback. Keep-alive is a property of the
/// client's buffer being reusable across requests, which is exactly what
/// [`Client::take_response`] leaves behind.
#[derive(Default)]
pub struct Client {
    rx: Vec<u8>,
    /// True while the request in flight was a HEAD (whose response has no body).
    head_in_flight: bool,
}

impl Client {
    pub fn new() -> Client {
        Client {
            rx: Vec::new(),
            head_in_flight: false,
        }
    }

    /// Build a `GET target` with a `host` header (and nothing else).
    pub fn get(&mut self, target: &[u8], host: &[u8]) -> Vec<u8> {
        self.request(b"GET", target, &[(b"host", host)], None)
    }

    /// Build an arbitrary request. Returns the bytes to write to the transport.
    pub fn request(
        &mut self,
        method: &[u8],
        target: &[u8],
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> Vec<u8> {
        self.head_in_flight = method.eq_ignore_ascii_case(b"HEAD");
        write_request(method, target, Version::Http11, headers, body)
    }

    /// Feed bytes read from the transport.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.rx.extend_from_slice(bytes);
    }

    /// Try to take one complete response out of the buffer. `Ok(None)` means the
    /// message has not fully arrived; any `Err` is fatal for the connection.
    pub fn take_response(&mut self) -> Result<Option<OwnedResponse>, Error> {
        let head = self.head_in_flight;
        let (resp, consumed) = {
            let r = match parse_response(&self.rx) {
                Ok(r) => r,
                Err(Error::Incomplete) => return Ok(None),
                Err(e) => return Err(e),
            };
            let kind = r.body(head)?;
            let (body, extra) = match kind {
                Body::None => (Vec::new(), 0),
                Body::Length(n) => {
                    if self.rx.len() < r.header_len + n {
                        return Ok(None);
                    }
                    (self.rx[r.header_len..r.header_len + n].to_vec(), n)
                }
                Body::Chunked => match chunked::decode(&self.rx[r.header_len..]) {
                    Ok((b, used)) => (b, used),
                    Err(Error::Incomplete) => return Ok(None),
                    Err(e) => return Err(e),
                },
                // Close-delimited: this driver has no close signal, so treat
                // whatever has arrived as the body and never report keep-alive.
                Body::Eof => {
                    let b = self.rx[r.header_len..].to_vec();
                    let n = b.len();
                    (b, n)
                }
            };
            let ka = r.keep_alive() && kind != Body::Eof;
            (
                OwnedResponse {
                    status: r.status,
                    reason: r.reason.to_vec(),
                    headers: own_headers(&r.headers),
                    body,
                    keep_alive: ka,
                },
                r.header_len + extra,
            )
        };
        self.rx.drain(..consumed);
        Ok(Some(resp))
    }

    /// Unconsumed received bytes (a pipelined next response, if any).
    pub fn buffered(&self) -> usize {
        self.rx.len()
    }
}

/// An HTTP/1.1 **server** over any byte transport: feed it bytes, take whole
/// requests out, answer with [`write_response`]. Same transport-agnostic shape as
/// [`Client`], so one implementation serves plaintext and HTTPS.
#[derive(Default)]
pub struct Server {
    rx: Vec<u8>,
}

impl Server {
    pub fn new() -> Server {
        Server { rx: Vec::new() }
    }

    /// Feed bytes read from the transport.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.rx.extend_from_slice(bytes);
    }

    /// Try to take one complete request. `Ok(None)` means incomplete; any `Err`
    /// must be answered with `400 Bad Request` and a close.
    pub fn take_request(&mut self) -> Result<Option<OwnedRequest>, Error> {
        let (req, consumed) = {
            let r = match parse_request(&self.rx) {
                Ok(r) => r,
                Err(Error::Incomplete) => return Ok(None),
                Err(e) => return Err(e),
            };
            let kind = r.body()?;
            let (body, extra) = match kind {
                Body::None | Body::Eof => (Vec::new(), 0),
                Body::Length(n) => {
                    if self.rx.len() < r.header_len + n {
                        return Ok(None);
                    }
                    (self.rx[r.header_len..r.header_len + n].to_vec(), n)
                }
                Body::Chunked => match chunked::decode(&self.rx[r.header_len..]) {
                    Ok((b, used)) => (b, used),
                    Err(Error::Incomplete) => return Ok(None),
                    Err(e) => return Err(e),
                },
            };
            (
                OwnedRequest {
                    method: r.method.to_vec(),
                    target: r.target.to_vec(),
                    version: r.version,
                    headers: own_headers(&r.headers),
                    body,
                    keep_alive: r.keep_alive(),
                },
                r.header_len + extra,
            )
        };
        self.rx.drain(..consumed);
        Ok(Some(req))
    }

    /// Unconsumed received bytes (a pipelined next request, if any).
    pub fn buffered(&self) -> usize {
        self.rx.len()
    }
}
