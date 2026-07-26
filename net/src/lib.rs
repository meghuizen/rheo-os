//! rheo-net: the greenfield network stack for rheo-os (docs/NETSTACK.md,
//! docs/NETWORKING.md). Portable userspace - `no_std` + alloc, no per-ISA code -
//! built for the three bare targets as a loaded ELF cell. It rides ON librheo's
//! raw-frame path (`librheo::net`: `OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC`), the
//! strand reactor, the one-shot timer, and the heap. It adds **no kernel object**
//! and **no per-ISA code** - it is pure parsing/building over the existing queue
//! ABI (the doctrine of docs/NETWORKING.md: the kernel owns queue plumbing, the
//! stack is a userspace cell).
//!
//! The full roadmap (N1-N8: TCP/CC, TLS, services, app protocols, perf substrate)
//! is docs/NETSTACK.md. This crate is **Phase N1a**, the first buildable slice -
//! the L2/L3 core:
//!
//! - [`eth`]: Ethernet II frame parse/build (dst/src MAC, ethertype, payload),
//!   zero-copy views over a buffer where possible.
//! - [`arp`]: ARP request/reply parse+build, an [`arp::ArpCache`] (IP -> MAC),
//!   and an async [`arp::resolve`] that sends a request over `librheo::net` and
//!   waits for the reply (bounded retries).
//! - [`ip`]: IPv4 + IPv6 header parse/build, the address types
//!   ([`ip::Ipv4Addr`]/[`ip::Ipv6Addr`]/[`ip::IpAddr`]), and the ones-complement
//!   Internet checksum ([`ip::checksum16`] / the reusable [`ip::Checksum`]
//!   accumulator, which UDP/ICMP reuse with a pseudo-header).
//!
//! **Phase N1b** adds L4/L3.5 over that core:
//! - [`udp`]: UDP datagram build/parse; the checksum over the IPv4/IPv6
//!   pseudo-header + header + payload via the N1a [`ip::Checksum`] accumulator;
//!   an async [`udp::UdpEndpoint`] (`send_to`/`recv_from`).
//! - [`icmp`]: ICMPv4 echo (ping) build/parse + an async [`icmp::IcmpEndpoint`],
//!   with the **IPv4 TTL hook** for a later traceroute.
//! - [`wire`]: the shared eth/ip framing + next-hop ARP resolution both L4
//!   protocols send/receive over (the TTL hook lives here).
//!
//! **Phase N1c** adds the caching resolver over that L4:
//! - [`dns`]: full DNS message build/parse (A/AAAA/CNAME + name-compression
//!   pointers, loop-bounded), an async caching [`dns::Resolver`] over
//!   [`udp::UdpEndpoint`], an LRU + TTL [`dns::Cache`], a [`dns::Blocklist`]
//!   (a from-scratch hash set + wildcard suffixes), and configurable resolvers +
//!   a static hosts table ([`dns::HostsTable`]).
//!
//! **Phase N1e** makes TTL / hop limit first-class and adds traceroute:
//! - [`ip`]: `DEFAULT_TTL`/`DEFAULT_HOP_LIMIT` (64) and the forwarding-plane
//!   [`ip::decrement_ttl`]/[`ip::decrement_hop_limit`] primitives (the
//!   router/firewall forward path - decrement, recompute the IPv4 checksum, drop
//!   + signal Time Exceeded at zero).
//! - [`icmp`]: ICMPv4 **Time Exceeded** (type 11) and ICMPv6 Time Exceeded (type
//!   3) build/parse, and `IcmpEndpoint::recv_trace`.
//! - [`trace`]: the TTL-increment traceroute state machine (deterministic core +
//!   a thin live driver), ICMP-echo probes correlated by sequence number.
//!
//! **Phase N2a** adds the native TCP transport over that L3/L4 core:
//! - [`tcp`]: the RFC 793 state machine ([`tcp::Connection`] +
//!   [`tcp::TcpStream`]/[`tcp::TcpListener`]) - three-way handshake, a sliding
//!   send/receive window with flow control, RFC 6298 RTO/RTT + Karn's algorithm,
//!   cumulative-ack retransmission, FIN teardown + TIME-WAIT, the TCP checksum via
//!   the N1a [`ip::Checksum`] accumulator, and a [`tcp::CongestionControl`] trait
//!   seam ([`tcp::FixedWindow`] for N2a; CUBIC/BBR are the N2b drop-in).
//! - [`timer`]: a [`timer::TimerWheel`] multiplexing many logical timers (per-
//!   connection RTO / TIME-WAIT) onto the reactor's single one-shot deadline.
//!
//! **Phase N2b** adds real congestion control over the N2a seam:
//! - [`cc`]: [`cc::Reno`] (RFC 5681 - slow start, AIMD, fast retransmit / fast
//!   recovery, RTO slow-start restart) and [`cc::Cubic`] (RFC 8312 - the cubic
//!   window in fixed point with an integer cube root), both drop-in
//!   [`tcp::CongestionControl`] impls wired into the send window. The trait grew
//!   `tick`/`on_dup_ack`/`ssthresh`/`in_recovery`/`set_mss` (default-implemented, so
//!   [`tcp::FixedWindow`] is unchanged), and [`tcp::Connection`] now detects
//!   duplicate ACKs and fast-retransmits on the 3rd.
//!
//! **Phase N2c** adds the two transports:
//! - [`smoltcp_cell`] (feature `smoltcp`): the blessed pure-Rust `no_std`
//!   transport integrated over the raw-frame NIC path - a [`smoltcp_cell::QueueDevice`]
//!   ([`smoltcp::phy::Device`]) whose RX/TX tokens carry the frames `librheo::net`
//!   ships, driven by [`smoltcp_cell::pump`]. Alongside the from-scratch stack,
//!   never replacing it.
//! - [`shard`]: the native **sharded** transport framing - a [`shard::Transport`]
//!   of N shards, connections hashed to shards by their [`shard::FourTuple`],
//!   shared-nothing (structural on the single CPU, not parallel - SMP is #27).
//!
//! **Phase N3a** adds the crypto primitive layer (feature `crypto`, off by
//! default):
//! - [`crypto`]: from-scratch ChaCha20-Poly1305 (RFC 8439) + doc-named RustCrypto
//!   SHA-2 / HKDF / X25519 / Ed25519 / AES-GCM, each proven against its RFC/NIST
//!   test vector; the two-randomness-class API ([`crypto::rand`] vs
//!   [`crypto::kdf`]) and the nonce-reuse guard ([`crypto::aead::SealingKey`]).
//!   The TLS 1.3 handshake over these primitives is N3b.
//!
//! **Phase N4a** adds the service-cell model - the keystone every later service
//! rides on:
//! - [`service`]: a [`service::Service`] holding **one cross-cell channel end per
//!   client** and running **one strand per client**, so a single network service
//!   cell serves many client cells concurrently (cooperative, single-CPU - SMP is
//!   task #27), plus the thin [`service::Client`] a spawned cell uses. Fan-out
//!   composes over librheo Phase J spawn/channel inheritance.
//!
//! **Phase N5a** adds the first **application protocols** over that transport
//! (docs/NETSTACK.md §19) - the gateway every remaining scenario (WAF/DPI,
//! S3-style storage, Arrow Flight, Kafka) rides on:
//! - [`http1`]: HTTP/1.1 - a **zero-copy** request/response codec (header names and
//!   values borrow the input buffer), `Content-Length` + **chunked** framing both
//!   directions, keep-alive, and a **smuggling-hardened** parser (both
//!   `Content-Length` and `Transfer-Encoding`, duplicate `Content-Length`, bare LF,
//!   whitespace before the colon, obs-fold, non-token names and oversized header
//!   blocks are each rejected with their own error), plus a transport-agnostic
//!   [`http1::Client`]/[`http1::Server`] pair driven over the synchronous
//!   [`tcp::Connection`] seam - or over the TLS record layer, which is how HTTPS
//!   composes.
//! - [`http2`]: HTTP/2 - the frame layer, the connection preface, the stream state
//!   machine, **connection- and stream-level flow control**, and **HPACK** (static
//!   table, dynamic table with size updates, and the RFC 7541 Appendix B Huffman
//!   code generated from the RFC text), proven against the RFC 7541 Appendix C
//!   known-answer vectors.
//!
//! Both live in the **always-compiled** (posture-independent) half of the crate:
//! HTTP is parsing and state machines, so it needs neither librheo nor the NIC.
//!
//! Still deferred (per docs/NETSTACK.md): TLS 1.3 (N3b); full NewReno partial-ACK
//! recovery, CUBIC HyStart / fast-convergence, and BBR; negative caching; and the
//! *live* ICMPv6 path (the v6 codec is unit-proven; SLIRP cannot generate v6
//! errors).

#![no_std]

extern crate alloc;

pub mod arp;
pub mod cc;
pub mod eth;
pub mod http1;
pub mod http2;
pub mod ip;
pub mod shard;
pub mod tcp;
pub mod udp;
pub mod wire;

// The **librheo-hosted** modules (feature `hosted`, on by default): every layer
// that reaches the NIC or the clock through librheo's async surface. Gating them
// out (`--no-default-features`) leaves the **codec posture** - pure parsing,
// framing, checksums and synchronous state machines - which is what makes the
// stack linkable into a *kernel* binary for the rheo-net N4b `svc::SocketOps`
// bridge (docs/NETSTACK.md N4b). librheo supplies a cell's `_start`, panic
// handler and global allocator, so a kernel cannot link it; the codec posture
// carries none of that. Nothing here is duplicated - the same `eth`/`ip`/`udp`/
// `tcp` code serves both postures.
#[cfg(feature = "hosted")]
pub mod dns;
#[cfg(feature = "hosted")]
pub mod icmp;
#[cfg(feature = "hosted")]
pub mod local;
#[cfg(feature = "hosted")]
pub mod service;
#[cfg(feature = "hosted")]
pub mod timer;
#[cfg(feature = "hosted")]
pub mod trace;

/// The smoltcp blessed transport cell (docs/NETSTACK.md §13, N2c). Gated behind
/// the `smoltcp` feature so the from-scratch stack + every existing test are
/// unaffected when it is off (the default). Present only when a cell opts in.
#[cfg(feature = "smoltcp")]
pub mod smoltcp_cell;

/// The N3a crypto primitive layer (docs/NETSTACK.md §3). Gated behind the
/// `crypto` feature so the base stack + every existing test are unaffected when
/// it is off (the default). Present only when a cell opts in.
#[cfg(feature = "crypto")]
pub mod crypto;

/// The N3b TLS 1.3 stack (docs/NETSTACK.md §15): the HKDF key schedule, the AEAD
/// record layer, the handshake state machine, and a minimal X.509 - all
/// from-scratch on the N3a `crypto` primitives. Gated behind the `tls` feature
/// (which implies `crypto`) so the base stack + every existing test are
/// unaffected when it is off (the default). Proven byte-for-byte against the RFC
/// 8448 known-answer trace.
#[cfg(feature = "tls")]
pub mod tls;
