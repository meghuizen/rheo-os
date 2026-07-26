//! `nethttp-demo` - the rheo-net Phase N5a proof cell (docs/NETSTACK.md §19):
//! **HTTP/1.1 + HTTP/2, client and server**, proven deterministically and
//! **network-free** in one cell. Exits `0x42` only if every check below passes, so
//! the `nethttp` kernel's exit-code assertion is the proof, on all three ISAs.
//!
//! 1. **HTTP/1.1 codec + zero copy.** A known request parses to exact
//!    method/target/version/headers, header lookup is case-insensitive, and the
//!    parsed header value's **pointer is inside the input buffer** - the borrow is
//!    asserted, not assumed.
//! 2. **Smuggling / robustness rejections.** Twenty-two malformed shapes each return
//!    their **specific** error: `Content-Length` + `Transfer-Encoding`, duplicate
//!    `Content-Length`, `5, 5`, `+5`, a non-`chunked` final coding, bare LF,
//!    `host : x`, obs-fold, a non-token name, a control byte in a value, an
//!    oversized header block, too many fields, a double space in the request line,
//!    and `HTTP/2.0` on a 1.x line. Chunked adds four more (`+5`, `0x5`, bare LF,
//!    a non-empty trailer).
//! 3. **The scan oracle.** The branchless SWAR byte scan and the plain scalar loop
//!    agree on 20,000 pseudo-random buffers for three different needles.
//! 4. **HTTP/1.1 client <-> server over real TCP.** Two `net::tcp` connections in
//!    one cell over the in-cell `VirtualLink` carry: a POST with headers and a body
//!    (exact round trip), a `Content-Length` response, a **chunked** response
//!    decoded to the exact bytes, a **second request reusing the same connection**
//!    (keep-alive), and a **404** error path.
//! 5. **HPACK against RFC 7541 Appendix C.** The RFC's own hex for C.1.1-C.1.3
//!    (integers), C.2.1-C.2.4 (each representation) and the C.3.1-C.3.3 /
//!    C.4.1-C.4.3 request **sequences** (the latter Huffman-coded) is decoded to
//!    exactly the RFC's header lists **and** re-encoded to exactly the RFC's bytes,
//!    with the dynamic table size checked against the RFC's stated 55 / 57 / 110 /
//!    164 at each step.
//! 6. **Huffman edge cases.** Round trip, bad padding rejected, an EOS symbol in
//!    the data rejected.
//! 7. **HTTP/2 over the same TCP pair.** Preface + SETTINGS exchange, a
//!    HEADERS+DATA request and response on stream 1, a **second concurrent
//!    stream**, a **flow-control-gated body** (the server's tiny initial window
//!    holds part of the body back until its WINDOW_UPDATE releases it),
//!    RST_STREAM, PING/PING-ACK and GOAWAY.
//! 8. **HTTPS composes.** One full HTTP/1.1 exchange runs **through the N3b TLS
//!    1.3 record layer** (our client and our server, real AEAD both ways), with
//!    **ALPN** negotiating `http/1.1`; `h2` is negotiated too, and a no-overlap
//!    offer correctly negotiates nothing.
//! 9. **A live GET is skipped with a reason** - QEMU's SLIRP provides DNS, TFTP
//!    and a gateway but **no HTTP server**, so there is nothing deterministic to
//!    fetch. Nothing is faked.
//!
//! Every RFC 7541 value below is the RFC's own published hex, hardcoded as the
//! expected answer and never computed by the code under test.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use librheo::println;
use rheo_net::http1::{self, Body, Version, scan};
use rheo_net::http2::{self, Event, StreamState, frame, hpack, huffman};
use rheo_net::ip::Ipv4Addr;
use rheo_net::tcp::{Connection, FixedWindow, VirtualLink};
use rheo_net::tls::{CipherSuite, ContentType, ServerIdentity, run_handshake_alpn};

/// Exit code on full success (the `nethttp` kernel asserts exactly this).
const OK_CODE: i32 = 0x42;

/// A concrete TCP connection over the trivial congestion controller.
type Conn = Connection<FixedWindow>;

// ---------------------------------------------------------------------------
// Compile-time hex (as in nettls-demo) so RFC values paste as published hex.
// ---------------------------------------------------------------------------

const fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}
const fn h<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (hexval(b[i * 2]) << 4) | hexval(b[i * 2 + 1]);
        i += 1;
    }
    out
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    if let Err(code) = run() {
        println!("nethttp-demo: FAIL at check {code}");
        return code;
    }
    println!("nethttp-demo: HTTP/1.1 + HTTP/2 (RFC 7541 HPACK KAT, smuggling-hardened) all OK");
    OK_CODE
}

fn run() -> Result<(), i32> {
    h1_codec_zero_copy()?;
    h1_smuggling_rejections()?;
    chunked_strictness()?;
    scan_oracle()?;
    h1_over_tcp()?;
    hpack_integers()?;
    hpack_c2_representations()?;
    hpack_c3_sequence()?;
    hpack_c4_sequence_huffman()?;
    huffman_edges()?;
    h2_over_tcp()?;
    https_over_tls()?;
    live_get_skip();
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. HTTP/1.1 codec + the zero-copy borrow proof
// ---------------------------------------------------------------------------

const SAMPLE_REQ: &[u8] = b"POST /api/search?q=1 HTTP/1.1\r\n\
Host: lattice.example\r\n\
Content-Type: application/json\r\n\
Accept: application/json\r\n\
Content-Length: 12\r\n\
\r\n\
{\"q\":\"rheo\"}";

fn h1_codec_zero_copy() -> Result<(), i32> {
    let buf = SAMPLE_REQ.to_vec();
    let req = http1::parse_request(&buf).map_err(|_| 100)?;
    if req.method != b"POST" {
        return Err(101);
    }
    if req.target != b"/api/search?q=1" {
        return Err(102);
    }
    if req.version != Version::Http11 {
        return Err(103);
    }
    if req.headers.len() != 4 {
        return Err(104);
    }
    // Case-insensitive lookup, both directions of casing.
    let host = req.headers.get(b"host").ok_or(105)?;
    if host != b"lattice.example" {
        return Err(106);
    }
    if req.headers.get(b"CONTENT-type") != Some(b"application/json".as_slice()) {
        return Err(107);
    }
    if req.headers.get(b"x-absent").is_some() {
        return Err(108);
    }
    // The framing decision.
    if req.body().map_err(|_| 109)? != Body::Length(12) {
        return Err(110);
    }
    if !req.keep_alive() {
        return Err(111);
    }
    if &buf[req.header_len..] != b"{\"q\":\"rheo\"}" {
        return Err(112);
    }

    // --- the zero-copy proof: the parsed value is a slice OF `buf` ---
    let base = buf.as_ptr() as usize;
    let end = base + buf.len();
    let vp = host.as_ptr() as usize;
    if !(vp >= base && vp + host.len() <= end) {
        return Err(113); // the header value was copied, not borrowed
    }
    // Every name and value must borrow, not just the one we looked up.
    for f in req.headers.as_slice() {
        for s in [f.name, f.value] {
            let p = s.as_ptr() as usize;
            if !(p >= base && p + s.len() <= end) {
                return Err(114);
            }
        }
    }
    if (req.method.as_ptr() as usize) < base || (req.target.as_ptr() as usize) >= end {
        return Err(115);
    }

    // A response, likewise.
    let rbuf =
        b"HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot foundextra"
            .to_vec();
    let resp = http1::parse_response(&rbuf).map_err(|_| 116)?;
    if resp.status != 404 || resp.reason != b"Not Found" {
        return Err(117);
    }
    if resp.body(false).map_err(|_| 118)? != Body::Length(9) {
        return Err(119);
    }
    if resp.keep_alive() {
        return Err(120); // `connection: close` must defeat HTTP/1.1 persistence
    }
    // A status line with no reason phrase is legal.
    let bare = b"HTTP/1.1 204\r\n\r\n".to_vec();
    let r2 = http1::parse_response(&bare).map_err(|_| 121)?;
    if r2.status != 204 || !r2.reason.is_empty() {
        return Err(122);
    }
    if r2.body(false).map_err(|_| 123)? != Body::None {
        return Err(124); // 204 never has a body
    }
    // An incomplete buffer is `Incomplete`, not an error.
    if http1::parse_request(b"GET / HTTP/1.1\r\nhost: a\r\n") != Err(http1::Error::Incomplete) {
        return Err(125);
    }
    println!("nethttp-demo: h1 codec + case-insensitive lookup + zero-copy borrow OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Smuggling / robustness rejections
// ---------------------------------------------------------------------------

/// Assert that `raw` parses but its **framing** is rejected with `want`.
fn expect_body_err(raw: &[u8], want: http1::Error, code: i32) -> Result<(), i32> {
    let r = http1::parse_request(raw).map_err(|_| code)?;
    match r.body() {
        Err(e) if e == want => Ok(()),
        _ => Err(code),
    }
}

/// Assert that `raw` fails to parse at all, with `want`.
fn expect_parse_err(raw: &[u8], want: http1::Error, code: i32) -> Result<(), i32> {
    match http1::parse_request(raw) {
        Err(e) if e == want => Ok(()),
        _ => Err(code),
    }
}

fn h1_smuggling_rejections() -> Result<(), i32> {
    use http1::Error as E;

    // (a) The headline desync: both framing headers on one message.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 5\r\ntransfer-encoding: chunked\r\n\r\n",
        E::BothLengthAndEncoding,
        200,
    )?;
    // The reversed order must also be rejected.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\ncontent-length: 5\r\n\r\n",
        E::BothLengthAndEncoding,
        201,
    )?;
    // (b) Two Content-Length fields - even when they agree.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 5\r\ncontent-length: 5\r\n\r\n",
        E::DuplicateContentLength,
        202,
    )?;
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 5\r\ncontent-length: 6\r\n\r\n",
        E::DuplicateContentLength,
        203,
    )?;
    // (c) A comma list inside one Content-Length.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 5, 5\r\n\r\n",
        E::BadContentLength,
        204,
    )?;
    // (d) A signed / non-decimal length.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: +5\r\n\r\n",
        E::BadContentLength,
        205,
    )?;
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 0x5\r\n\r\n",
        E::BadContentLength,
        206,
    )?;
    // (e) A Transfer-Encoding whose final coding is not chunked.
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked, identity\r\n\r\n",
        E::BadTransferEncoding,
        207,
    )?;
    expect_body_err(
        b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\ntransfer-encoding: chunked\r\n\r\n",
        E::BadTransferEncoding,
        208,
    )?;
    // (f) A bare LF terminating the request line.
    expect_parse_err(b"GET / HTTP/1.1\nhost: a\r\n\r\n", E::BareLf, 209)?;
    // ... and terminating a header line.
    expect_parse_err(b"GET / HTTP/1.1\r\nhost: a\n\r\n", E::BareLf, 210)?;
    // (g) Whitespace before the colon.
    expect_parse_err(
        b"GET / HTTP/1.1\r\nhost : a\r\n\r\n",
        E::SpaceBeforeColon,
        211,
    )?;
    // (h) An obs-fold continuation line.
    expect_parse_err(
        b"GET / HTTP/1.1\r\nhost: a\r\n more\r\n\r\n",
        E::ObsFold,
        212,
    )?;
    // (i) A non-token byte in a header name.
    expect_parse_err(b"GET / HTTP/1.1\r\nho(st: a\r\n\r\n", E::BadHeaderName, 213)?;
    expect_parse_err(b"GET / HTTP/1.1\r\n: a\r\n\r\n", E::BadHeaderName, 214)?;
    // (j) A control byte in a header value.
    expect_parse_err(
        b"GET / HTTP/1.1\r\nx: a\x01b\r\n\r\n",
        E::BadHeaderValue,
        215,
    )?;
    // (k) An oversized header block.
    {
        let mut big = Vec::from(&b"GET / HTTP/1.1\r\nx: "[..]);
        big.extend(core::iter::repeat_n(b'a', 20 * 1024));
        big.extend_from_slice(b"\r\n\r\n");
        expect_parse_err(&big, E::HeaderBlockTooLarge, 216)?;
    }
    // (l) Too many header fields.
    {
        let mut many = Vec::from(&b"GET / HTTP/1.1\r\n"[..]);
        for _ in 0..70 {
            many.extend_from_slice(b"x-pad: v\r\n");
        }
        many.extend_from_slice(b"\r\n");
        expect_parse_err(&many, E::TooManyHeaders, 217)?;
    }
    // (m) A double space in the request line.
    expect_parse_err(b"GET  / HTTP/1.1\r\n\r\n", E::BadRequestLine, 218)?;
    expect_parse_err(b"GET /\r\n\r\n", E::BadRequestLine, 219)?;
    // (n) A version this parser does not speak on a 1.x line.
    expect_parse_err(b"GET / HTTP/2.0\r\n\r\n", E::BadVersion, 220)?;
    // A smuggling attempt on a bodiless response is still rejected, not ignored.
    {
        let raw =
            b"HTTP/1.1 204 No Content\r\ncontent-length: 5\r\ntransfer-encoding: chunked\r\n\r\n";
        let r = http1::parse_response(raw).map_err(|_| 221)?;
        if r.body(false) != Err(E::BothLengthAndEncoding) {
            return Err(222);
        }
    }
    println!(
        "nethttp-demo: 22 request-smuggling / robustness shapes each rejected with its own error"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 2b. Chunked strictness (and the happy path)
// ---------------------------------------------------------------------------

fn chunked_strictness() -> Result<(), i32> {
    use http1::Error as E;
    // Happy path, including a chunk extension we accept and ignore.
    const CHUNKED: &[u8] = b"5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\n\r\n";
    let (body, used) = http1::chunked::decode(CHUNKED).map_err(|_| 230)?;
    if body != b"hello, world" {
        return Err(231);
    }
    if used != CHUNKED.len() {
        return Err(232);
    }
    // Encode / decode round trip with small chunks.
    let payload = b"rheo-net chunked round trip over the in-cell transport";
    let enc = http1::chunked::encode(payload, 7);
    let (dec, n) = http1::chunked::decode(&enc).map_err(|_| 234)?;
    if dec != payload || n != enc.len() {
        return Err(235);
    }
    // Truncated is Incomplete, not an error.
    if http1::chunked::decode(b"5\r\nhel").map(|_| ()) != Err(E::Incomplete) {
        return Err(236);
    }
    // Strictness: signed size, hex prefix, bare LF, non-empty trailer.
    if http1::chunked::decode(b"+5\r\nhello\r\n0\r\n\r\n").map(|_| ()) != Err(E::BadChunkSize) {
        return Err(237);
    }
    if http1::chunked::decode(b"0x5\r\nhello\r\n0\r\n\r\n").map(|_| ()) != Err(E::BadChunkSize) {
        return Err(238);
    }
    if http1::chunked::decode(b"5\nhello\r\n0\r\n\r\n").map(|_| ()) != Err(E::BareLf) {
        return Err(239);
    }
    if http1::chunked::decode(b"0\r\nx-trailer: v\r\n\r\n").map(|_| ())
        != Err(E::ChunkTrailerUnsupported)
    {
        return Err(240);
    }
    println!("nethttp-demo: chunked round trip + 4 strictness rejections OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. The scan oracle: branchless SWAR == scalar
// ---------------------------------------------------------------------------

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

fn scan_oracle() -> Result<(), i32> {
    let mut st = 0x5eed_1234u64;
    // Bytes biased toward the interesting set so hits land at every offset.
    let alphabet: &[u8] = b"ab\r\n: /HTP1.0\t\x00xyz";
    for _ in 0..20_000 {
        let len = (lcg(&mut st) % 80) as usize;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push(alphabet[(lcg(&mut st) as usize) % alphabet.len()]);
        }
        for needle in *b"\n:\r" {
            if scan::find_byte(&buf, needle) != scan::find_byte_scalar(&buf, needle) {
                return Err(300);
            }
        }
    }
    // Token / OWS / case helpers.
    if scan::token_end(b"content-length") != 14 {
        return Err(301);
    }
    if scan::token_end(b"content length") != 7 {
        return Err(302);
    }
    if scan::trim_ows(b" \t value \t ") != b"value" {
        return Err(303);
    }
    if !scan::eq_ignore_case(b"Content-Length", b"content-length") {
        return Err(304);
    }
    if scan::eq_ignore_case(b"a", b"ab") {
        return Err(305);
    }
    println!("nethttp-demo: SWAR scan == scalar oracle over 20000 fuzz buffers (3 needles) OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. HTTP/1.1 client <-> server over the in-cell TCP pair
// ---------------------------------------------------------------------------

/// Drive two TCP connections to quiescence over the virtual link, advancing the
/// logical clock to the next deadline whenever neither has immediate output. This
/// is the `nettcp` pump - the deterministic in-cell transport.
fn pump(a: &mut Conn, b: &mut Conn, link: &mut VirtualLink, now: &mut u64) {
    for _ in 0..200_000 {
        let mut progressed = false;
        while let Some(s) = a.poll(*now) {
            link.transfer(&s, b, *now);
            progressed = true;
        }
        while let Some(s) = b.poll(*now) {
            link.transfer(&s, a, *now);
            progressed = true;
        }
        if progressed {
            continue;
        }
        let next = match (a.poll_at(), b.poll_at()) {
            (Some(x), Some(y)) => x.min(y),
            (Some(x), None) => x,
            (None, Some(y)) => y,
            (None, None) => return,
        };
        *now = if next > *now { next } else { *now + 1 };
    }
}

/// Write every byte of `data` into `conn`, pumping when the send buffer fills.
fn tcp_write_all(
    conn: &mut Conn,
    peer: &mut Conn,
    link: &mut VirtualLink,
    now: &mut u64,
    data: &[u8],
) {
    let mut off = 0;
    while off < data.len() {
        let n = conn.write(&data[off..]);
        off += n;
        pump(conn, peer, link, now);
        if n == 0 && off < data.len() {
            // The peer is not draining; the deterministic scenarios never hit this.
            return;
        }
    }
}

/// Drain everything readable from `conn` into a byte sink.
fn tcp_drain(conn: &mut Conn, sink: &mut Vec<u8>) {
    let mut buf = [0u8; 2048];
    loop {
        let n = conn.read(&mut buf);
        if n == 0 {
            return;
        }
        sink.extend_from_slice(&buf[..n]);
    }
}

/// The demo server's routing table: `/hello` (Content-Length), `/stream`
/// (chunked), anything else 404.
fn route(req: &http1::OwnedRequest) -> Vec<u8> {
    match req.target.as_slice() {
        b"/api/search" => http1::write_response(
            200,
            b"OK",
            Version::Http11,
            &[(b"content-type", b"application/json")],
            br#"{"hits":2,"engine":"rheo-net"}"#,
        ),
        b"/stream" => http1::write_response_chunked(
            200,
            b"OK",
            &[(b"content-type", b"text/plain")],
            b"chunk one; chunk two; chunk three; the whole body reassembles exactly",
            9,
        ),
        _ => http1::write_response(
            404,
            b"Not Found",
            Version::Http11,
            &[(b"content-type", b"text/plain")],
            b"no such route",
        ),
    }
}

fn h1_over_tcp() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let (cport, sport) = (50_000u16, 80u16);
    let mut ctcp: Conn = Connection::connect(cip, cport, sip, sport, 0x0000_1000);
    let mut stcp: Conn = Connection::listen(sip, sport, cip, cport, 0x0000_9000);
    let mut link = VirtualLink::new();
    let mut now = 0u64;
    pump(&mut ctcp, &mut stcp, &mut link, &mut now);
    if !ctcp.is_established() || !stcp.is_established() {
        return Err(400);
    }

    let mut client = http1::Client::new();
    let mut server = http1::Server::new();

    // --- exchange 1: POST with headers + body, Content-Length response ---
    let body = br#"{"q":"rheo","limit":2}"#;
    let bytes = client.request(
        b"POST",
        b"/api/search",
        &[
            (b"host", b"lattice.example"),
            (b"accept", b"application/json"),
            (b"content-type", b"application/json"),
            (b"x-trace", b"n5a-1"),
        ],
        Some(body),
    );
    tcp_write_all(&mut ctcp, &mut stcp, &mut link, &mut now, &bytes);
    let mut sink = Vec::new();
    tcp_drain(&mut stcp, &mut sink);
    server.feed(&sink);
    let req = server.take_request().map_err(|_| 401)?.ok_or(402)?;
    if req.method != b"POST" || req.target != b"/api/search" {
        return Err(403);
    }
    if req.body != body {
        return Err(404);
    }
    if req.header(b"X-Trace") != Some(b"n5a-1".as_slice()) {
        return Err(405);
    }
    if !req.keep_alive {
        return Err(406);
    }
    let resp_bytes = route(&req);
    tcp_write_all(&mut stcp, &mut ctcp, &mut link, &mut now, &resp_bytes);
    let mut csink = Vec::new();
    tcp_drain(&mut ctcp, &mut csink);
    client.feed(&csink);
    let resp = client.take_response().map_err(|_| 407)?.ok_or(408)?;
    if resp.status != 200 || resp.reason != b"OK" {
        return Err(409);
    }
    if resp.body != br#"{"hits":2,"engine":"rheo-net"}"# {
        return Err(410);
    }
    if resp.header(b"Content-Type") != Some(b"application/json".as_slice()) {
        return Err(411);
    }
    if !resp.keep_alive {
        return Err(412);
    }
    if client.buffered() != 0 {
        return Err(413); // the response was consumed exactly, no trailing bytes
    }

    // --- exchange 2: keep-alive reuse of the SAME connection, chunked response ---
    let bytes2 = client.request(b"GET", b"/stream", &[(b"host", b"lattice.example")], None);
    tcp_write_all(&mut ctcp, &mut stcp, &mut link, &mut now, &bytes2);
    sink.clear();
    tcp_drain(&mut stcp, &mut sink);
    server.feed(&sink);
    let req2 = server.take_request().map_err(|_| 414)?.ok_or(415)?;
    if req2.target != b"/stream" || !req2.body.is_empty() {
        return Err(416);
    }
    let resp_bytes2 = route(&req2);
    // The response really is chunked on the wire.
    if !window_contains(&resp_bytes2, b"transfer-encoding: chunked") {
        return Err(417);
    }
    tcp_write_all(&mut stcp, &mut ctcp, &mut link, &mut now, &resp_bytes2);
    csink.clear();
    tcp_drain(&mut ctcp, &mut csink);
    client.feed(&csink);
    let resp2 = client.take_response().map_err(|_| 418)?.ok_or(419)?;
    if resp2.status != 200 {
        return Err(420);
    }
    if resp2.body != b"chunk one; chunk two; chunk three; the whole body reassembles exactly" {
        return Err(421); // the chunked body must reassemble byte-exactly
    }
    // The TCP connection was never closed - one connection carried both exchanges.
    if !ctcp.is_established() || !stcp.is_established() {
        return Err(422);
    }

    // --- exchange 3: the 404 error path, still on the same connection ---
    let bytes3 = client.request(b"GET", b"/nope", &[(b"host", b"lattice.example")], None);
    tcp_write_all(&mut ctcp, &mut stcp, &mut link, &mut now, &bytes3);
    sink.clear();
    tcp_drain(&mut stcp, &mut sink);
    server.feed(&sink);
    let req3 = server.take_request().map_err(|_| 423)?.ok_or(424)?;
    let resp_bytes3 = route(&req3);
    tcp_write_all(&mut stcp, &mut ctcp, &mut link, &mut now, &resp_bytes3);
    csink.clear();
    tcp_drain(&mut ctcp, &mut csink);
    client.feed(&csink);
    let resp3 = client.take_response().map_err(|_| 425)?.ok_or(426)?;
    if resp3.status != 404 || resp3.reason != b"Not Found" || resp3.body != b"no such route" {
        return Err(427);
    }

    // Graceful teardown.
    ctcp.close();
    pump(&mut ctcp, &mut stcp, &mut link, &mut now);
    stcp.close();
    pump(&mut ctcp, &mut stcp, &mut link, &mut now);
    println!(
        "nethttp-demo: h1 client<->server over real TCP - POST+body, chunked, keep-alive reuse, 404 OK"
    );
    Ok(())
}

/// True if `needle` occurs anywhere in `hay`.
fn window_contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// 5. HPACK against RFC 7541 Appendix C
// ---------------------------------------------------------------------------

/// RFC 7541 C.1.1-C.1.3: the prefix-integer representation.
fn hpack_integers() -> Result<(), i32> {
    // C.1.1: 10 with a 5-bit prefix fits in the prefix -> 0b000_01010.
    let mut v = Vec::new();
    hpack::encode_int(&mut v, 5, 0x00, 10);
    if v != [0x0a] {
        return Err(500);
    }
    let mut p = 0;
    if hpack::decode_int(&v, &mut p, 5) != Ok(10) || p != 1 {
        return Err(501);
    }
    // C.1.2: 1337 with a 5-bit prefix -> prefix 31, then 154, then 10.
    let mut v = Vec::new();
    hpack::encode_int(&mut v, 5, 0x00, 1337);
    if v != [0x1f, 0x9a, 0x0a] {
        return Err(502);
    }
    let mut p = 0;
    if hpack::decode_int(&v, &mut p, 5) != Ok(1337) || p != 3 {
        return Err(503);
    }
    // C.1.3: 42 at an octet boundary (8-bit prefix) -> 0b00101010.
    let mut v = Vec::new();
    hpack::encode_int(&mut v, 8, 0x00, 42);
    if v != [0x2a] {
        return Err(504);
    }
    let mut p = 0;
    if hpack::decode_int(&v, &mut p, 8) != Ok(42) || p != 1 {
        return Err(505);
    }
    // A truncated continuation run is an error, not a wrap.
    let mut p = 0;
    if hpack::decode_int(&[0x1f, 0x9a], &mut p, 5) != Err(hpack::HpackError::Truncated) {
        return Err(506);
    }
    let mut p = 0;
    let overflow = [
        0x1fu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ];
    if hpack::decode_int(&overflow, &mut p, 5) != Err(hpack::HpackError::IntegerOverflow) {
        return Err(507);
    }
    Ok(())
}

/// Compare a decoded header list against expected `(name, value)` pairs.
fn expect_headers(
    got: &[(Vec<u8>, Vec<u8>)],
    want: &[(&[u8], &[u8])],
    code: i32,
) -> Result<(), i32> {
    if got.len() != want.len() {
        return Err(code);
    }
    for (g, w) in got.iter().zip(want.iter()) {
        if g.0.as_slice() != w.0 || g.1.as_slice() != w.1 {
            return Err(code);
        }
    }
    Ok(())
}

/// RFC 7541 C.2.1-C.2.4: each header field representation, independently.
fn hpack_c2_representations() -> Result<(), i32> {
    // C.2.1 Literal Header Field with Incremental Indexing.
    const C21: [u8; 26] = h("400a637573746f6d2d6b65790d637573746f6d2d686561646572");
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    let got = dec.decode(&C21).map_err(|_| 510)?;
    expect_headers(&got, &[(b"custom-key", b"custom-header")], 511)?;
    if dec.table().size() != 55 {
        return Err(512); // the RFC prints "Table size: 55"
    }
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, false);
    if enc.encode(&[(b"custom-key", b"custom-header")]) != C21 {
        return Err(513);
    }
    if enc.table().size() != 55 {
        return Err(514);
    }

    // C.2.2 Literal Header Field without Indexing (indexed name 4 = :path).
    const C22: [u8; 14] = h("040c2f73616d706c652f70617468");
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    let got = dec.decode(&C22).map_err(|_| 515)?;
    expect_headers(&got, &[(b":path", b"/sample/path")], 516)?;
    if dec.table().size() != 0 {
        return Err(517); // "Dynamic table (after decoding): empty"
    }
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, false);
    if enc.encode_with_modes(&[(b":path", b"/sample/path", hpack::Mode::NoIndex)]) != C22 {
        return Err(518);
    }
    if enc.table().size() != 0 {
        return Err(519);
    }

    // C.2.3 Literal Header Field Never Indexed.
    const C23: [u8; 17] = h("100870617373776f726406736563726574");
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    let got = dec.decode(&C23).map_err(|_| 520)?;
    expect_headers(&got, &[(b"password", b"secret")], 521)?;
    if dec.table().size() != 0 {
        return Err(522);
    }
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, false);
    if enc.encode_with_modes(&[(b"password", b"secret", hpack::Mode::NeverIndex)]) != C23 {
        return Err(523);
    }

    // C.2.4 Indexed Header Field (static index 2).
    const C24: [u8; 1] = h("82");
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    let got = dec.decode(&C24).map_err(|_| 524)?;
    expect_headers(&got, &[(b":method", b"GET")], 525)?;
    if dec.table().size() != 0 {
        return Err(526);
    }
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, false);
    if enc.encode(&[(b":method", b"GET")]) != C24 {
        return Err(527);
    }

    // Robustness: index 0 and an out-of-range index are rejected.
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    if dec.decode(&[0x80]) != Err(hpack::HpackError::BadIndex) {
        return Err(528);
    }
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    if dec.decode(&[0xff, 0x00]) != Err(hpack::HpackError::BadIndex) {
        return Err(529);
    }
    // A table size update larger than the decoder's maximum is rejected.
    let mut dec = hpack::Decoder::new(256);
    if dec.decode(&[0x3f, 0xe1, 0x1f]) != Err(hpack::HpackError::BadTableSizeUpdate) {
        return Err(530);
    }
    // A legal size update is applied and evicts.
    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    dec.decode(&C21).map_err(|_| 531)?;
    if dec.table().size() != 55 {
        return Err(532);
    }
    let mut shrink = Vec::new();
    hpack::encode_int(&mut shrink, 5, 0x20, 0);
    if dec.decode(&shrink).is_err() {
        return Err(533);
    }
    if dec.table().size() != 0 || dec.table().capacity() != 0 {
        return Err(534); // capacity 0 must evict everything
    }
    println!("nethttp-demo: HPACK RFC 7541 C.1 + C.2.1-C.2.4 decode+encode byte-for-byte OK");
    Ok(())
}

/// The C.3 / C.4 request sequence, shared: the three header lists the RFC encodes.
fn c3_lists() -> [Vec<(&'static [u8], &'static [u8])>; 3] {
    [
        alloc::vec![
            (b":method".as_slice(), b"GET".as_slice()),
            (b":scheme", b"http"),
            (b":path", b"/"),
            (b":authority", b"www.example.com"),
        ],
        alloc::vec![
            (b":method".as_slice(), b"GET".as_slice()),
            (b":scheme", b"http"),
            (b":path", b"/"),
            (b":authority", b"www.example.com"),
            (b"cache-control", b"no-cache"),
        ],
        alloc::vec![
            (b":method".as_slice(), b"GET".as_slice()),
            (b":scheme", b"https"),
            (b":path", b"/index.html"),
            (b":authority", b"www.example.com"),
            (b"custom-key", b"custom-value"),
        ],
    ]
}

/// RFC 7541 C.3.1-C.3.3: three consecutive request header lists on one
/// connection, **without** Huffman coding. Proves the dynamic table evolves
/// identically in encoder and decoder (indices 62 and 63 are only reachable if it
/// does) and that its reported size matches the RFC's 57 / 110 / 164.
fn hpack_c3_sequence() -> Result<(), i32> {
    const C31: [u8; 20] = h("828684410f7777772e6578616d706c652e636f6d");
    const C32: [u8; 14] = h("828684be58086e6f2d6361636865");
    const C33: [u8; 29] = h("828785bf400a637573746f6d2d6b65790c637573746f6d2d76616c7565");
    let wire: [&[u8]; 3] = [&C31, &C32, &C33];
    let sizes = [57usize, 110, 164];
    let lists = c3_lists();

    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, false);
    for i in 0..3 {
        let got = dec.decode(wire[i]).map_err(|_| 540 + i as i32 * 10)?;
        expect_headers(&got, &lists[i], 541 + i as i32 * 10)?;
        if dec.table().size() != sizes[i] {
            return Err(542 + i as i32 * 10);
        }
        let re = enc.encode(&lists[i]);
        if re.as_slice() != wire[i] {
            return Err(543 + i as i32 * 10);
        }
        if enc.table().size() != sizes[i] {
            return Err(544 + i as i32 * 10);
        }
    }
    println!(
        "nethttp-demo: HPACK RFC 7541 C.3.1-C.3.3 sequence (indices 62/63, sizes 57/110/164) OK"
    );
    Ok(())
}

/// RFC 7541 C.4.1-C.4.3: the same three requests **with** Huffman-coded literals.
/// This is where the generated Appendix B table is exercised end to end.
fn hpack_c4_sequence_huffman() -> Result<(), i32> {
    // RFC 7541 C.4.1: `8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff`
    const C41: [u8; 17] = h("828684418cf1e3c2e5f23a6ba0ab90f4ff");
    // C.4.2: `8286 84be 5886 a8eb 1064 9cbf`
    const C42: [u8; 12] = h("828684be5886a8eb10649cbf");
    // C.4.3: `8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf`
    const C43: [u8; 24] = h("828785bf408825a849e95ba97d7f8925a849e95bb8e8b4bf");
    let wire: [&[u8]; 3] = [&C41, &C42, &C43];
    let sizes = [57usize, 110, 164];
    let lists = c3_lists();

    let mut dec = hpack::Decoder::new(hpack::DEFAULT_TABLE_SIZE);
    // Huffman ON - this is the only difference from C.3.
    let mut enc = hpack::Encoder::new(hpack::DEFAULT_TABLE_SIZE, true);
    for i in 0..3 {
        let got = dec.decode(wire[i]).map_err(|_| 560 + i as i32 * 10)?;
        expect_headers(&got, &lists[i], 561 + i as i32 * 10)?;
        if dec.table().size() != sizes[i] {
            return Err(562 + i as i32 * 10);
        }
        let re = enc.encode(&lists[i]);
        if re.as_slice() != wire[i] {
            return Err(563 + i as i32 * 10);
        }
        if enc.table().size() != sizes[i] {
            return Err(564 + i as i32 * 10);
        }
    }
    println!(
        "nethttp-demo: HPACK RFC 7541 C.4.1-C.4.3 sequence (Huffman literals) byte-for-byte OK"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. Huffman edge cases
// ---------------------------------------------------------------------------

fn huffman_edges() -> Result<(), i32> {
    // Round trip over every byte value.
    let all: Vec<u8> = (0..=255u8).collect();
    let enc = huffman::encode(&all);
    if huffman::decode(&enc).map_err(|_| 600)? != all {
        return Err(601);
    }
    if enc.len() != huffman::encoded_len(&all) {
        return Err(602);
    }
    // The RFC 7541 C.4.1 value: "www.example.com" Huffman-codes to 12 octets.
    const WWW: [u8; 12] = h("f1e3c2e5f23a6ba0ab90f4ff");
    if huffman::encode(b"www.example.com") != WWW {
        return Err(603);
    }
    if huffman::decode(&WWW).map_err(|_| 604)? != b"www.example.com" {
        return Err(605);
    }
    // C.4.2's value: "no-cache" -> 6 octets.
    const NOCACHE: [u8; 6] = h("a8eb10649cbf");
    if huffman::encode(b"no-cache") != NOCACHE {
        return Err(606);
    }
    if huffman::decode(&NOCACHE).map_err(|_| 607)? != b"no-cache" {
        return Err(608);
    }
    // Padding that is not all ones is rejected ('a' is 5 bits: 00011 + 111 pad).
    if huffman::encode(b"a") != [0x1f] {
        return Err(609);
    }
    if huffman::decode(&[0x18]).is_ok() {
        return Err(610);
    }
    // Padding longer than 7 bits is rejected.
    if huffman::decode(&[0x1f, 0xff]).is_ok() {
        return Err(611);
    }
    // A decoded EOS symbol is rejected (30 one bits).
    if huffman::decode(&[0xff, 0xff, 0xff, 0xff]).is_ok() {
        return Err(612);
    }
    println!("nethttp-demo: HPACK Huffman round trip + padding/EOS rejections OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// 7. HTTP/2 over the in-cell TCP pair
// ---------------------------------------------------------------------------

/// Shuttle bytes between two h2 endpoints across the TCP virtual link until no
/// more move. Each round: take each endpoint's queued output, write it into its
/// TCP connection, pump the link, and feed whatever arrived into the peer.
fn h2_shuttle(
    cc: &mut http2::Connection,
    sc: &mut http2::Connection,
    ctcp: &mut Conn,
    stcp: &mut Conn,
    link: &mut VirtualLink,
    now: &mut u64,
) -> Result<(), i32> {
    for _ in 0..64 {
        let cout = cc.take_out();
        let sout = sc.take_out();
        let moved = !cout.is_empty() || !sout.is_empty();
        if !cout.is_empty() {
            tcp_write_all(ctcp, stcp, link, now, &cout);
        }
        if !sout.is_empty() {
            tcp_write_all(stcp, ctcp, link, now, &sout);
        }
        pump(ctcp, stcp, link, now);
        let mut to_server = Vec::new();
        tcp_drain(stcp, &mut to_server);
        let mut to_client = Vec::new();
        tcp_drain(ctcp, &mut to_client);
        if !to_server.is_empty() {
            sc.on_bytes(&to_server).map_err(|_| 700)?;
        }
        if !to_client.is_empty() {
            cc.on_bytes(&to_client).map_err(|_| 701)?;
        }
        if !moved && to_server.is_empty() && to_client.is_empty() {
            return Ok(());
        }
    }
    Ok(())
}

/// Drain an endpoint's events, returning them.
fn drain_events(c: &mut http2::Connection) -> Vec<Event> {
    let mut v = Vec::new();
    while let Some(e) = c.next_event() {
        v.push(e);
    }
    v
}

fn h2_over_tcp() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let (cport, sport) = (50_001u16, 443u16);
    let mut ctcp: Conn = Connection::connect(cip, cport, sip, sport, 0x0000_2000);
    let mut stcp: Conn = Connection::listen(sip, sport, cip, cport, 0x0000_a000);
    let mut link = VirtualLink::new();
    let mut now = 0u64;
    pump(&mut ctcp, &mut stcp, &mut link, &mut now);

    // The server advertises a deliberately tiny per-stream receive window, so a
    // body larger than it MUST be held back by flow control.
    const SERVER_WINDOW: u32 = 16;
    let mut cc = http2::Connection::client(65_535);
    let mut sc = http2::Connection::server(SERVER_WINDOW);

    // --- preface + SETTINGS exchange ---
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let sev = drain_events(&mut sc);
    let cev = drain_events(&mut cc);
    if !sev.contains(&Event::Settings) {
        return Err(710); // the server never saw the client's SETTINGS
    }
    if !cev.contains(&Event::Settings) {
        return Err(711);
    }
    if !sev.contains(&Event::SettingsAck) || !cev.contains(&Event::SettingsAck) {
        return Err(712); // both SETTINGS must have been acknowledged
    }

    // --- stream 1: HEADERS + a flow-control-gated DATA body ---
    let s1 = cc.open_stream();
    if s1 != 1 {
        return Err(713); // a client's first stream is 1 (RFC 9113 5.1.1)
    }
    cc.send_headers(
        s1,
        &[
            (b":method", b"POST"),
            (b":scheme", b"https"),
            (b":path", b"/api/search"),
            (b":authority", b"lattice.example"),
            (b"content-type", b"application/json"),
        ],
        false,
    );
    const BODY: &[u8] = br#"{"q":"rheo","limit":2,"pad":"aaaaaaaa"}"#;
    if BODY.len() <= SERVER_WINDOW as usize {
        return Err(714); // the scenario requires a body larger than the window
    }
    cc.send_data(s1, BODY, true);
    // Only `SERVER_WINDOW` bytes may have gone out; the rest is queued.
    if cc.stream_pending(s1) != BODY.len() - SERVER_WINDOW as usize {
        return Err(715);
    }
    if cc.stream_send_window(s1) != Some(0) {
        return Err(716);
    }
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;

    let mut got_headers: Option<Vec<(Vec<u8>, Vec<u8>)>> = None;
    let mut body_seen: Vec<u8> = Vec::new();
    let mut end_seen = false;
    for e in drain_events(&mut sc) {
        match e {
            Event::Headers {
                stream_id,
                headers,
                end_stream,
            } => {
                if stream_id != 1 || end_stream {
                    return Err(717);
                }
                got_headers = Some(headers);
            }
            Event::Data {
                stream_id,
                data,
                end_stream,
            } => {
                if stream_id != 1 {
                    return Err(718);
                }
                body_seen.extend_from_slice(&data);
                end_seen |= end_stream;
            }
            _ => {}
        }
    }
    let hdrs = got_headers.ok_or(719)?;
    if hdrs.len() != 5 || hdrs[0].0 != b":method" || hdrs[0].1 != b"POST" {
        return Err(720);
    }
    if hdrs[3].1 != b"lattice.example" {
        return Err(721); // HPACK carried the authority through intact
    }
    // Exactly the window's worth arrived, and the stream is NOT ended yet.
    if body_seen.len() != SERVER_WINDOW as usize {
        return Err(722);
    }
    if end_seen {
        return Err(723);
    }

    // --- the server opens the window; the queued remainder flows ---
    let credit = (BODY.len() - SERVER_WINDOW as usize) as u32;
    sc.send_window_update(1, credit);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let mut window_update_seen = false;
    for e in drain_events(&mut cc) {
        if let Event::WindowUpdate {
            stream_id: 1,
            increment,
        } = e
            && increment == credit
        {
            window_update_seen = true;
        }
    }
    if !window_update_seen {
        return Err(724);
    }
    if cc.stream_pending(s1) != 0 {
        return Err(725); // the credit must have released the queued bytes
    }
    for e in drain_events(&mut sc) {
        if let Event::Data {
            stream_id,
            data,
            end_stream,
        } = e
        {
            if stream_id != 1 {
                return Err(726);
            }
            body_seen.extend_from_slice(&data);
            end_seen |= end_stream;
        }
    }
    if body_seen != BODY {
        return Err(727); // the whole body reassembled byte-exactly
    }
    if !end_seen {
        return Err(728);
    }
    if sc.stream_state(1) != Some(StreamState::HalfClosedRemote) {
        return Err(729);
    }

    // --- the server answers on stream 1 ---
    const RESP: &[u8] = br#"{"hits":2,"engine":"rheo-net","proto":"h2"}"#;
    sc.send_headers(
        1,
        &[(b":status", b"200"), (b"content-type", b"application/json")],
        false,
    );
    // The client's window is large, so the whole response goes out at once.
    sc.send_data(1, RESP, true);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let mut status = None;
    let mut cbody = Vec::new();
    let mut cend = false;
    for e in drain_events(&mut cc) {
        match e {
            Event::Headers {
                stream_id, headers, ..
            } => {
                if stream_id != 1 {
                    return Err(730);
                }
                status = headers
                    .iter()
                    .find(|(n, _)| n == b":status")
                    .map(|(_, v)| v.clone());
            }
            Event::Data {
                data, end_stream, ..
            } => {
                cbody.extend_from_slice(&data);
                cend |= end_stream;
            }
            _ => {}
        }
    }
    if status.as_deref() != Some(b"200".as_slice()) {
        return Err(731);
    }
    if cbody != RESP || !cend {
        return Err(732);
    }
    if cc.stream_state(1) != Some(StreamState::Closed) {
        return Err(733);
    }

    // --- a second, concurrent stream ---
    let s3 = cc.open_stream();
    if s3 != 3 {
        return Err(734);
    }
    cc.send_headers(
        s3,
        &[
            (b":method", b"GET"),
            (b":scheme", b"https"),
            (b":path", b"/health"),
            (b":authority", b"lattice.example"),
        ],
        true,
    );
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let mut saw3 = false;
    for e in drain_events(&mut sc) {
        if let Event::Headers {
            stream_id: 3,
            headers,
            end_stream: true,
        } = e
            && headers
                .iter()
                .any(|(n, v)| n == b":path" && v == b"/health")
        {
            saw3 = true;
        }
    }
    if !saw3 {
        return Err(735);
    }
    sc.send_headers(3, &[(b":status", b"200")], false);
    sc.send_data(3, b"ok", true);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let mut ok3 = Vec::new();
    for e in drain_events(&mut cc) {
        if let Event::Data {
            stream_id, data, ..
        } = e
            && stream_id == 3
        {
            ok3.extend_from_slice(&data);
        }
    }
    if ok3 != b"ok" {
        return Err(736);
    }
    if cc.stream_count() < 2 {
        return Err(737); // both streams were tracked on one connection
    }

    // --- RST_STREAM on a third stream ---
    let s5 = cc.open_stream();
    cc.send_headers(s5, &[(b":method", b"GET"), (b":path", b"/slow")], true);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    drain_events(&mut sc);
    sc.send_rst_stream(s5, frame::err::CANCEL);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    let mut reset_ok = false;
    for e in drain_events(&mut cc) {
        if e == (Event::StreamReset {
            stream_id: s5,
            error_code: frame::err::CANCEL,
        }) {
            reset_ok = true;
        }
    }
    if !reset_ok {
        return Err(738);
    }
    if cc.stream_state(s5) != Some(StreamState::Closed) {
        return Err(739);
    }

    // --- PING / PING ACK ---
    let ping = *b"rheo-net";
    cc.send_ping(ping);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    if !drain_events(&mut sc).iter().any(|e| {
        *e == Event::Ping {
            payload: ping,
            ack: false,
        }
    }) {
        return Err(740);
    }
    if !drain_events(&mut cc).iter().any(|e| {
        *e == Event::Ping {
            payload: ping,
            ack: true,
        }
    }) {
        return Err(741); // the peer must auto-acknowledge a PING
    }

    // --- GOAWAY ---
    sc.send_goaway(frame::err::NO_ERROR);
    h2_shuttle(&mut cc, &mut sc, &mut ctcp, &mut stcp, &mut link, &mut now)?;
    if !drain_events(&mut cc).iter().any(
        |e| matches!(e, Event::GoAway { error_code, .. } if *error_code == frame::err::NO_ERROR),
    ) {
        return Err(742);
    }
    if !cc.goaway_received() {
        return Err(743);
    }

    // --- protocol errors are errors, not silent acceptance ---
    // A bad client preface.
    let mut bad = http2::Connection::server(65_535);
    if bad.on_bytes(b"GET / HTTP/1.1\r\n\r\n").is_ok() {
        return Err(744);
    }
    // A WINDOW_UPDATE of zero.
    let mut peer = http2::Connection::client(65_535);
    peer.take_out();
    if peer.on_bytes(&frame::build_window_update(0, 0)).is_ok() {
        return Err(745);
    }
    // Server push is refused outright.
    let mut peer = http2::Connection::client(65_535);
    peer.take_out();
    if peer
        .on_bytes(&frame::build(
            frame::kind::PUSH_PROMISE,
            4,
            1,
            &[0, 0, 0, 3],
        ))
        .is_ok()
    {
        return Err(746);
    }
    // A PRIORITY frame is accepted and ignored.
    let mut peer = http2::Connection::client(65_535);
    peer.take_out();
    if peer
        .on_bytes(&frame::build(
            frame::kind::PRIORITY,
            0,
            1,
            &[0, 0, 0, 0, 15],
        ))
        .is_err()
    {
        return Err(747);
    }
    if peer.event_count() != 0 {
        return Err(748);
    }

    println!(
        "nethttp-demo: h2 preface+SETTINGS, HEADERS+DATA, 2 concurrent streams, \
         WINDOW_UPDATE-gated body, RST/PING/GOAWAY OK"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 8. HTTPS: HTTP/1.1 through the N3b TLS 1.3 record layer, ALPN-negotiated
// ---------------------------------------------------------------------------

/// The same Ed25519 test certificate + seed the `nettls` proof uses (generated
/// once by openssl; a real cert, hardcoded, never committed as a fixture).
const TEST_CERT_DER: [u8; 328] = h(concat!(
    "308201443081f7a00302010202141dd6277cb3a4f05ff4e8e30b2b5b63d9a143",
    "7cad300506032b657030183116301406035504030c0d7268656f2d6e65742074",
    "657374301e170d3236303732363132323831375a170d33363037323331323238",
    "31375a30183116301406035504030c0d7268656f2d6e65742074657374302a30",
    "0506032b657003210032142a51fffcac956ca6a32441e8d9f2927e1324fd7855",
    "6450953999e52b8f35a3533051301d0603551d0e0416041415eb797fa50c6787",
    "52ef8d2a0b79ad9b6218f3b0301f0603551d2304183016801415eb797fa50c67",
    "8752ef8d2a0b79ad9b6218f3b0300f0603551d130101ff040530030101ff3005",
    "06032b6570034100285db26f12e04ff7b805ff1a2445e36d2dfabf35943b7847",
    "574b702bf1ee9e12d2f4276dc6bcee5bcf0a7de0782099b68cfd19d5733b6f5e",
    "d5c7113ceaa22708"
));
const TEST_CERT_SEED: [u8; 32] =
    h("f53d8117f9e99f188570860eebf9bc5349a378b95dd8ab18659d5d738e12f3d9");

fn https_over_tls() -> Result<(), i32> {
    let server_id = ServerIdentity {
        cert_der: TEST_CERT_DER.to_vec(),
        ed25519_seed: TEST_CERT_SEED,
    };

    // --- ALPN: http/1.1 is negotiated, and both sides agree ---
    let mut out = run_handshake_alpn(
        CipherSuite::Aes128GcmSha256,
        &server_id,
        &[b"h2", b"http/1.1"],
        &[b"http/1.1"],
    )
    .map_err(|_| 800)?;
    if out.alpn.as_deref() != Some(b"http/1.1".as_slice()) {
        return Err(801);
    }
    if !(out.cert_verified && out.server_finished_ok && out.client_finished_ok && out.keys_match) {
        return Err(802);
    }

    // --- one full HTTP/1.1 exchange, every byte through the AEAD record layer ---
    let mut client = http1::Client::new();
    let mut server = http1::Server::new();
    let req_bytes = client.request(
        b"GET",
        b"/secure",
        &[(b"host", b"lattice.example"), (b"accept", b"text/plain")],
        None,
    );
    let record = out
        .client_app_write
        .encrypt(ContentType::ApplicationData, &req_bytes);
    // The request must genuinely be encrypted, not passed through.
    if window_contains(&record, b"/secure") {
        return Err(803);
    }
    let (ct, pt) = out.server_app_read.decrypt(&record).map_err(|_| 804)?;
    if ct != ContentType::ApplicationData {
        return Err(805);
    }
    server.feed(&pt);
    let req = server.take_request().map_err(|_| 806)?.ok_or(807)?;
    if req.method != b"GET" || req.target != b"/secure" {
        return Err(808);
    }
    if req.header(b"host") != Some(b"lattice.example".as_slice()) {
        return Err(809);
    }

    const SECRET_BODY: &[u8] = b"http/1.1 over tls 1.3, composed";
    let resp_bytes = http1::write_response(
        200,
        b"OK",
        Version::Http11,
        &[(b"content-type", b"text/plain")],
        SECRET_BODY,
    );
    let record2 = out
        .server_app_write
        .encrypt(ContentType::ApplicationData, &resp_bytes);
    if window_contains(&record2, SECRET_BODY) {
        return Err(810);
    }
    let (_, pt2) = out.client_app_read.decrypt(&record2).map_err(|_| 811)?;
    client.feed(&pt2);
    let resp = client.take_response().map_err(|_| 812)?.ok_or(813)?;
    if resp.status != 200 || resp.body != SECRET_BODY {
        return Err(814);
    }
    // A tampered record must still fail the AEAD - HTTP over TLS inherits that.
    let mut bad = out
        .server_app_write
        .encrypt(ContentType::ApplicationData, &resp_bytes);
    bad[9] ^= 0x01;
    if out.client_app_read.decrypt(&bad).is_ok() {
        return Err(815);
    }

    // --- ALPN: h2 is negotiated when both offer it (this is what makes
    // h2-over-TLS real rather than assumed) ---
    let h2out = run_handshake_alpn(
        CipherSuite::ChaCha20Poly1305Sha256,
        &server_id,
        &[b"h2", b"http/1.1"],
        &[b"h2", b"http/1.1"],
    )
    .map_err(|_| 816)?;
    if h2out.alpn.as_deref() != Some(b"h2".as_slice()) {
        return Err(817);
    }
    // --- and nothing is negotiated when the lists do not overlap ---
    let none = run_handshake_alpn(
        CipherSuite::Aes128GcmSha256,
        &server_id,
        &[b"h2"],
        &[b"http/1.1"],
    )
    .map_err(|_| 818)?;
    if none.alpn.is_some() {
        return Err(819);
    }
    // --- and the no-ALPN handshake is unchanged (N3b compatibility) ---
    let plain =
        rheo_net::tls::run_handshake(CipherSuite::Aes128GcmSha256, &server_id).map_err(|_| 820)?;
    if plain.alpn.is_some() {
        return Err(821);
    }

    println!(
        "nethttp-demo: HTTPS - h1 exchange through the TLS 1.3 record layer + ALPN http/1.1 & h2 OK"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 9. The live path: skipped, with a reason
// ---------------------------------------------------------------------------

fn live_get_skip() {
    println!(
        "nethttp-demo: live GET SKIPPED - QEMU SLIRP offers DNS (10.0.2.3), TFTP and a \
         gateway (10.0.2.2) but no HTTP server, so there is no deterministic, \
         network-free endpoint to fetch; nothing is faked"
    );
}
