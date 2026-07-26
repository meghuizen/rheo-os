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
//! Still deferred (per docs/NETSTACK.md): the smoltcp blessed cell + the sharded
//! transport + real congestion control (N2b); `local` (the AF_UNIX-equivalent
//! zero-copy transport), the Linux AF_UNIX personality, negative caching, and the
//! *live* ICMPv6 path (the v6 codec is unit-proven; SLIRP cannot generate v6
//! errors).

#![no_std]

extern crate alloc;

pub mod arp;
pub mod dns;
pub mod eth;
pub mod icmp;
pub mod ip;
pub mod local;
pub mod tcp;
pub mod timer;
pub mod trace;
pub mod udp;
pub mod wire;
