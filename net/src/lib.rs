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
//! Still deferred (per docs/NETSTACK.md): `local` (the AF_UNIX-equivalent
//! zero-copy transport), the caching `dns` client, ICMPv6, and full traceroute.

#![no_std]

extern crate alloc;

pub mod arp;
pub mod eth;
pub mod icmp;
pub mod ip;
pub mod udp;
pub mod wire;
