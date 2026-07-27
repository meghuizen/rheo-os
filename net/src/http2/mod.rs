//! `net::http2` - HTTP/2 (RFC 9113) with HPACK (RFC 7541), rheo-net Phase N5a
//! (docs/NETSTACK.md §19). Like [`crate::http1`] it is pure codec + a synchronous
//! state machine: **no kernel object, no per-ISA code, no dependency**, and it
//! compiles in both rheo-net postures.
//!
//! - [`frame`]: the 9-octet frame header and every frame this slice speaks -
//!   DATA, HEADERS, SETTINGS, WINDOW_UPDATE, PING, RST_STREAM, GOAWAY,
//!   CONTINUATION, plus PRIORITY parsed-and-ignored.
//! - [`huffman`]: the RFC 7541 Appendix B Huffman code, **generated** from the RFC
//!   text (never hand-transcribed) and decoded canonically, with the padding / EOS
//!   rules enforced.
//! - [`hpack`]: the static table, a size-bounded dynamic table, prefix integers,
//!   string literals, and an encoder + decoder proven against the RFC 7541
//!   Appendix C known-answer vectors.
//! - [`conn`]: the connection - preface, SETTINGS exchange, the stream state
//!   machine, and **connection- and stream-level flow control**.
//!
//! ## Transport: prior-knowledge h2c, or ALPN `h2` over TLS
//! Nothing here knows about the transport, so h2 runs over the native TCP seam
//! (prior-knowledge h2c) or over the N3b TLS 1.3 record layer. For the TLS case
//! N5a added a **minimal ALPN** (RFC 7301) to the handshake -
//! [`crate::tls::run_handshake_alpn`] offers a protocol list in the ClientHello and
//! the server echoes its choice in EncryptedExtensions - so `h2` is genuinely
//! negotiated rather than assumed. The HTTP/1.1 `Upgrade: h2c` dance is **not**
//! implemented (it is deprecated by RFC 9113 §3.1 anyway).

pub mod conn;
pub mod frame;
pub mod hpack;
pub mod huffman;

pub use conn::{Connection, Event, H2Error, PREFACE, StreamState};
pub use hpack::{Decoder, Encoder, HpackError, Mode};
