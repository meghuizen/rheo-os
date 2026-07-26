# rheo-net: the greenfield network stack

**Status:** Building. Phase **N1a** (the L2/L3 core), **N1b's L4** (UDP + ICMP),
**N1c's caching DNS client**, **N1e's TTL / hop-limit + traceroute**, **N1d's
local sockets** (native `net::local` + Linux AF_UNIX), **N2a's native TCP core**,
**N2b's congestion control** (Reno + CUBIC), **N2c's two transports** (the smoltcp
blessed cell + the native sharded framing - §13), the **L8-INET** personality
slice (AF_INET/AF_INET6 sockets + a minimal epoll over the **loopback** interface -
§10(C), docs/LINUX-COMPAT.md), **N3a's crypto primitive layer** (from-scratch
ChaCha20-Poly1305 + doc-named RustCrypto SHA-2/HKDF/X25519/Ed25519/AES-GCM, each
RFC/NIST-vector-proven - §14), and **N3b's TLS 1.3** (a from-scratch handshake +
record layer + minimal X.509 over N3a, proven byte-for-byte against the RFC 8448
known-answer trace - §15) are done; the full
roadmap (N1-N8) is below. This document is the architecture + roadmap + crypto posture;
`docs/NETWORKING.md` holds the doctrine (the kernel owns queue plumbing + grant
checks + steering, and no network stack).

## 0. Position

rheo-net is the userspace network foundation: async-queue, capability-native,
zero-copy, built as **portable userspace cells** over the existing raw-frame NIC
path (`librheo::net`: `OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC` over virtio-net). It
adds **no kernel object** in N1-N5 (it composes over the existing queue ABI) and
**no per-ISA code** (no `cfg(target_arch)` - the stack is pure portable Rust).

It must span, from **one composable foundation**, four deployment classes:
embedded (raw frames + minimal UDP), internet-exchange / firewall / DDoS / WAF at
scale, extreme low-latency / HFT hot paths, and data-warehouse throughput (Arrow
Flight, Kafka). Two steers shape the design: **not everything lives in one stack**
(composable, profile-selected), and **lean on audited libraries where fitting**
(device drivers and crypto), every crate named in a doc per the no-deps rule.

## 1. Principles (from sDDF + the OS's doctrine)

- **Everything above Ethernet is a userspace cell.** The kernel stays queue
  plumbing + grant checks + (later, if earned) hardware steering.
- **Datapath ABI = the existing queue-pair**, disciplined toward the sDDF
  **free+active two-ring** credit loop with **offset-only descriptors** (sealed
  grants), and the **armed-doorbell + re-check + deferred-notify** batching folded
  into the librheo reactor (batching is ABI, not per-protocol). *(N1a uses the
  plain raw-frame path; the two-ring discipline lands with the perf substrate.)*
- **Trust boundary = a swappable copier**: copy across untrusted boundaries, alias
  sealed grants across trusted ones (true zero-copy) - a per-connection choice.
- **Shared-nothing sharding**: one transport instance per cell/core, connections
  hashed to cells, cross-shard over explicit queue-pairs. This is also the DDoS
  isolation story - a flood burns only the target tenant's arena/budget.
- **Partition, then go branchless**: branchless hot paths only work on
  *homogeneous* input. The RX-virtualiser partitions each batch by pipeline
  (protocol / flow-class / offload-state) so each downstream path sees one packet
  shape; the branch removal + SIMD/mask arithmetic ride on top. **The partition is
  the load-bearing trick, not the mask arithmetic.**
- **Composability / "skip parts of the stack"**: a connection **selects a datapath
  at connect time** - `local` (AF_UNIX/native zero-copy IPC, no IP) vs `wire` (full
  stack). Layers are opt-in per **profile** feature gate, so no deployment pays for
  what it does not use.

## 2. Layered module map (the `net/` crate)

A new `no_std` + alloc workspace crate, mirroring `librheo/` + `json/`, built for
the three bare targets as a loaded ELF cell, feature-gated per profile.

- **L2**: `eth` (Ethernet II frame parse/build), `arp` (cache + request/reply +
  async resolve); `wire` (the shared eth/ip framing + next-hop ARP resolution the
  L4 protocols send/receive over, holding the IPv4 TTL hook); later the sDDF
  driver / RX-virtualiser (demux) / copier split; jumbo frames.
- **L3**: `ip` (IPv4 + IPv6 parse/build + the ones-complement Internet checksum);
  `icmp` (ICMPv4 echo/ping + the traceroute TTL hook); later ICMPv6, IGMP/MLD
  (multicast), fragmentation.
- **L4**: `udp` (datagram build/parse + the pseudo-header checksum + an async
  `UdpEndpoint`); then `tcp` with RTT/RTO + **pluggable congestion control**
  (CUBIC/BBR-shaped trait); the local fast-path selector.
- **`local` / AF_UNIX**: the zero-copy cross-cell transport (over the Phase E/J
  `ipc::Channel` + sealed grants) AND a Linux-personality **AF_UNIX** milestone
  (SOCK_STREAM/DGRAM + `SCM_RIGHTS`) backed by the L6 pipe/channel machinery.
- **Services**: a caching DNS client (LRU+TTL cache, huge blocklists, configurable
  resolvers, host config); DHCP + zeroconf (userspace); NTP (PTP/NTS later).
- **Security transports**: TLS 1.2/1.3, WireGuard, IPsec; **keys-as-capabilities**
  (programmed into a queue, never readable back); the inline-NIC-crypto seam
  designed, offload deferred.
- **App protocols**: HTTP/1.1 + HTTP/2 (HTTP/3 over QUIC later), gRPC, **Apache
  Arrow Flight** (rides the zero-copy grant + columnar `store`/`io`), Kafka client.
- **Deferred dataplane**: eBPF/WASM filter stage, DDoS pre-steering, DPU/NIC
  offload, RDMA east-west.

### Profile feature gates

`net/Cargo.toml` declares `embedded` / `edge` (default) / `hft` / `warehouse`.
In N1a every profile pulls the same L2/L3 core (the modules are always compiled);
the gates exist now so later layers attach per profile without churning the public
surface.

## 3. Crypto posture (hybrid)

- Reuse the hand-written constant-time ChaCha20 (`kernel/src/rng/chacha.rs`); add
  a from-scratch **Poly1305** -> ChaCha20-Poly1305.
- **Doc-name audited crates** for the rest (each named here per the no-deps rule),
  `no_std` where possible, wrapped behind rheo-net's own async/capability API so
  the external surface stays native: RustCrypto **aes-gcm**, **sha2**, **hkdf**,
  **x25519-dalek**, **ed25519-dalek**; a **rustls**-class TLS; a
  **boringtun**-core-class WireGuard. **smoltcp** is the blessed correctness-first
  transport for control/low-rate cells (Redox precedent); the HFT/warehouse hot
  lines use a native, sharded, zero-copy transport instead.
- **The primitive layer is built and vector-proven** (Phase N3a, §14). The final
  pinned inventory - all behind the `net` crate's `crypto` feature (off by
  default; built separately by xtask), `no_std`, `default-features = false`:

  | Primitive | Provider | Version | RFC/NIST vector |
  | --- | --- | --- | --- |
  | ChaCha20 block/keystream | **from-scratch** (our own) | - | RFC 8439 §2.3.2 |
  | Poly1305 MAC | **from-scratch** | - | RFC 8439 §2.5.2 |
  | ChaCha20-Poly1305 AEAD | **from-scratch** | - | RFC 8439 §2.8.2 |
  | SHA-256 / SHA-384 | `sha2` | 0.10.8 | NIST / RFC 6234 |
  | HKDF (HMAC-SHA256) | `hkdf` | 0.12.4 | RFC 5869 TC1 |
  | X25519 | `x25519-dalek` | 2.0.1 | RFC 7748 §5.2 / §6.1 |
  | Ed25519 | `ed25519-dalek` | 2.1.1 | RFC 8032 §7.1 |
  | AES-128/256-GCM | `aes-gcm` | 0.10.3 | GCM-spec / NIST TC4 / TC16 |

  The dalek crates pull `curve25519-dalek` 4.1.3; aes-gcm pulls `aes` 0.8.4 +
  `ghash`/`polyval` 0.6.2. **Build note (the N2c smoltcp-style risk, cleared):**
  the crates build `no_std` on all three bare targets, but on
  `x86_64-unknown-none` the default target features select intrinsics backends
  (AES-NI / CLMUL / AVX2 SIMD) that **miscompile under LLVM** ("Do not know how to
  split the result of this operator"). The crypto build therefore forces the
  **software** backends via `RUSTFLAGS` cfgs (`aes_force_soft`,
  `polyval_force_soft`, `curve25519_dalek_backend="serial"`, applied uniformly on
  all three ISAs) - the scalar portable path this posture wants anyway. No crate
  needed a from-scratch fallback; all five are crate-backed as named.
- **TLS 1.3 (N3b, §15) added no new pinned crate**: the handshake, key schedule,
  record layer, and minimal X.509 are **from-scratch over the N3a primitives**
  (the architecture choice, §15) - rustls was evaluated and set aside. So the
  pinned inventory above is unchanged by N3b.
- **Two randomness classes, never conflated** (mixing them is a silent
  nonce-reuse break): the ChaCha20 fast DRBG is for **non-secret** randomness only
  (cookies, DNS transaction ids, hash seeds, backoff jitter). **Protocol key
  schedules** (TLS 1.3, Noise/WireGuard, IPsec) derive keys with **HKDF over the
  handshake transcript**, keyed from the **attested per-cell DRBG** - never the
  fast RNG.
- **Fork + checkpoint/restore are nonce-reuse hazards**: a restored/forked cell
  must reseed before any AEAD; a `(key, nonce)` pair must never replay across a
  checkpoint. Keys are per-cell, keys-as-capabilities, never checkpointed as
  plaintext.
- Arch **crypto-instruction dispatch** (AES-NI/VAES, ARM CE, RISC-V vector crypto)
  behind the existing Arch trait seam; a **scalar portable fallback always
  present** (the `json` precedent - CPU is the benchmark every offload must beat).

## 4. Kernel-surface posture (userspace-first, earned hardening)

Build N1-N5 over existing surface. Add substrate **only as a phase earns it**,
each docs-first per ARCHITECTURE.md 6 and reusing prior work: NIC **RX interrupt**
(built on the Phase D IRQ path - **done in N2d**, §16), **zero-copy grant DMA to the wire** +
header/payload split, virtio-net **offload + multiqueue** negotiation with
**attach-time offload validation** (run correctness vectors and **silently disable
broken offloads**, falling back to the CPU path), **larger/elastic grants** (raise
the 16-page cap), **service fan-out** (multi-peer `SYS_CONNECT`), a transport
**timer wheel** over `SYS_ARM_TIMER`, and last - only if line-rate multi-tenant
fan-out demands it - the **socket `ObjectKind` + steering grants** (the `Stream`
enum slot already reserves "console/pipe/socket").

## 5. Phased roadmap

Each phase = optional substrate + net modules + a real 3-ISA proof over QEMU
SLIRP (deterministic where possible); docs-first; icount perf; all pre-existing
kernels stay green.

- **N1 - Local fast-path + L2-L4 core + caching DNS.** Split into slices:
  - **N1a (done): L2/L3 core.** `eth` + `arp` (cache + async resolve) + `ip`
    (IPv4 + IPv6 + checksum). Proof in §6.
  - **N1b (L4 done): UDP + ICMP.** `udp` (pseudo-header checksum + async
    `UdpEndpoint`) and `icmp` (ICMPv4 echo + the traceroute TTL hook), proven by
    a DNS query over UDP to SLIRP's `10.0.2.3:53` and a ping to the gateway
    `10.0.2.2` (§7).
  - **N1c (done): the caching DNS client.** `dns` - full message build/parse
    (A/AAAA/CNAME + name-compression pointers, loop-bounded), an async caching
    `Resolver` over `udp`, an LRU + TTL `Cache`, a `Blocklist` (a from-scratch
    hash set + wildcard suffixes), and configurable resolvers + a static hosts
    table (§8).
  - **N1e (done): first-class TTL / hop limit + traceroute.** `ip` gains the
    default 64 (TTL + IPv6 hop limit) and the forwarding-plane
    `decrement_ttl`/`decrement_hop_limit` primitives (the router/firewall path);
    `icmp` gains ICMP Time Exceeded (v4 type 11 + the v6 type 3 codec); `trace` is
    the TTL-increment traceroute state machine (§9).
  - **N1d (done): local sockets.** `net::local` - a native zero-copy cell-to-cell
    transport over `librheo::ipc` + sealed grants (no IP/Ethernet) + the datapath
    selector (local vs wire); and **Linux-personality AF_UNIX** (SOCK_STREAM
    socketpair + bind/listen/connect/accept over a name registry) backed by the L6
    cross-cell ring - the first slice of the "L8" socket surface (§10,
    docs/LINUX-COMPAT.md L8).
- **N2 - TCP + congestion control + the two transports** (native sharded + the
  smoltcp cell); proof: a TCP echo / HTTP GET to SLIRP. Split into slices:
  - **N2a (done): the native TCP core + a timer wheel.** `tcp` (the RFC 793 state
    machine + RFC 6298 RTO/RTT + Karn, sliding-window flow control, cumulative-ack
    retransmission, FIN teardown + TIME-WAIT, the TCP checksum, a `CongestionControl`
    trait seam) and `timer` (a timer wheel multiplexing many logical timers onto the
    reactor's single one-shot). Proven **deterministically in-cell** - two endpoints
    over a virtual link drive the full lifecycle incl. a drop/RTO recovery (§11).
  - **N2b (done): real congestion control - Reno + CUBIC.** `cc` (RFC 5681 Reno and
    RFC 8312 CUBIC as drop-in `CongestionControl` impls over the N2a seam), wired
    into the send window, with fast-retransmit dup-ACK handling added to `tcp`.
    Proven **deterministically in-cell** - integer cwnd trajectories pinned against
    oracles + a real fast-retransmit-before-RTO scenario (§12).
  - **N2c (done): the two transports.** The smoltcp blessed correctness cell
    (integrated over the raw-frame NIC path, a live smoltcp UDP round trip to
    SLIRP's DNS) and the native sharded transport framing (connections hashed to
    shards, shared-nothing). §13.
  - **N2d (done): true async receive.** The **NIC RX interrupt** + a
    park-until-frame kernel verb (`SYS_WAIT_NET`) + a reactor network slot, so
    `librheo::net::recv` **parks** instead of re-polling `OP_NET_RX` - a cell
    waiting for a packet no longer burns a core. Pulled forward from N6 because
    the transports of N2a-N2c are the first code that genuinely waits on the wire
    (§16).
- **N3 - TLS 1.3 + HTTPS.** Crypto crates wired; keys-as-capabilities. Split:
  - **N3a (done): the crypto primitive layer** (§14). The AEAD/hash/KDF/
    key-exchange/signature primitives the security transports need, each proven
    against its RFC/NIST test vector on 3 ISAs: from-scratch ChaCha20-Poly1305 +
    doc-named RustCrypto SHA-2 / HKDF / X25519 / Ed25519 / AES-GCM, the
    two-randomness-class API, and the nonce-reuse guard.
  - **N3b (done): the TLS 1.3 handshake + record layer + minimal X.509** over
    N3a's primitives (§15). From-scratch (not rustls); the key schedule / record
    layer / handshake / Ed25519-X.509 are proven byte-for-byte against the RFC
    8448 known-answer trace. HTTPS-live is deferred (N3c/N4).
- **N4 - Service-cell model + fan-out + host services** (DHCP + zeroconf + NTP).
  Split into slices:
  - **N4a (done): the network service cell + concurrent fan-out** (§17) - the
    keystone. One long-lived service cell holds **one cross-cell channel per
    client** and runs **one strand per client**, serving N clients concurrently.
    Everything later rides here.
  - **N4b (done): the remote-INET bridge for the personality** (§18) - a Linux
    cell's non-loopback `AF_INET` connect/send/recv is forwarded to a registered
    **`svc::SocketOps`** table (the `svc::FileOps` precedent - a bridge, no kernel
    object), whose datapath is `rheo-net` in a new librheo-free **codec** posture
    driving virtio-net. An **unmodified static-glibc binary** does a real DNS round
    trip to SLIRP's resolver and a real remote TCP connect on all three ISAs.
    Routing it to the **N4a service cell** instead is the documented end state,
    blocked on N4a's name-based rendezvous.
  - **N4c: host services** - DHCP + zeroconf + NTP clients as service cells.
- **N5 - App protocols.** HTTP/2, gRPC, Arrow Flight (warehouse), Kafka.
- **N6 - Perf substrate.** (NIC RX IRQ landed early in **N2d**, §16.) Zero-copy DMA + offload/multiqueue/RSS,
  timer wheel; the socket/steering kernel object if earned; DDoS-isolation proof.
- **N7 - WireGuard + IPsec + QUIC/HTTP3 + multicast/IGMP + ICMP polish** (core
  traceroute + TTL/hop-limit landed early in **N1e**, §9; N7 keeps IGMP/MLD, PMTU,
  and the live ICMPv6 path).
- **N8 - Inline NIC TLS offload** (the keys-as-capabilities payoff): program
  session keys into a NIC TX/RX queue as a capability, encrypted zero-copy fetch
  removes Kafka's encryption cliff. Hardware-only, so the QEMU proof is the
  key-programming path + a software-AEAD fallback, with the line-rate number
  labeled a lab figure.
- **Deferred (documented)**: eBPF/WASM dataplane, DDoS pre-steering, DPU offload,
  RDMA east-west, PTP/NTS authenticated time.

## 6. Phase N1a (done): the L2/L3 core

### What the `net/` crate ships

- **`net::eth`** - Ethernet II frame parse/build. `Frame` is a zero-copy view
  (dst/src MAC, ethertype, payload slice) over a borrowed buffer; `Header` +
  `build_frame` write into a caller buffer. The MAC type is `librheo::net::Mac`,
  re-exported so the whole stack shares one address type.
- **`net::arp`** - `ArpPacket` parse/build (28-byte Ethernet/IPv4), `build_request`
  (a full broadcast request frame, built through `eth` + `ArpPacket`, not
  hand-laid bytes), an `ArpCache` (IPv4 -> MAC, a `BTreeMap` with insert/lookup and
  a minimal `lookup_fresh` TTL), and `async fn resolve` (send the request over
  `librheo::net`, poll for the reply parsing via `eth`/`arp`, cache it; bounded
  retries).
- **`net::ip`** - `Ipv4Addr` / `Ipv6Addr` / `IpAddr`; `Ipv4Header` (20-byte,
  no-options) build/parse with a correct header checksum + `verify_checksum`;
  `Ipv6Header` (40-byte) build/parse; and the **ones-complement Internet checksum**
  (RFC 1071): `checksum16` + a reusable `Checksum` accumulator.

### The Internet checksum (correctness-critical)

`Checksum` sums the data as big-endian 16-bit words into a `u32`, folds the
end-around carry (`while (sum >> 16) != 0`), and takes the one's complement. It is
an **accumulator** that carries a leftover odd byte across `add` calls, so a
pseudo-header + payload summed as separate slices matches one contiguous buffer -
this is exactly the shape UDP/TCP/ICMP need in N1b (they prepend an IP
pseudo-header). The path is **scalar and portable** (the correctness requirement);
any future SIMD path keeps this scalar routine as its oracle (the `json`
precedent). Validated against a **known-good value**: the RFC/Wikipedia IPv4
example header checksums to `0xB861`, asserted in the proof.

### The proof (`netcore` test kernel, all 3 ISAs)

A `netcore-demo` cell (loaded from an ELF into a cell with a mapped queue pair,
mirroring `librheonet`) over QEMU SLIRP + virtio-net:

1. reads the NIC MAC via `librheo::net`, then **resolves the SLIRP gateway
   `10.0.2.2` via `net::arp::resolve`** - a genuine ARP request out / reply in
   through the virtqueues, built and parsed with `net::eth`/`net::arp` (this
   replaces `librheo-net`'s hand-built ARP frame) - and asserts the cache is
   populated (the second lookup hits it);
2. asserts `checksum16` of the known IPv4 example equals `0xB861`, then does an
   IPv4 header build -> parse -> **checksum-validate** round trip and asserts a
   flipped byte fails validation;
3. does an IPv6 header build -> parse round trip.

It exits `0x42` only if every step passes; the kernel is untouched. Same SLIRP +
virtio-net QEMU wiring as `librheonet` (virtio-mmio on arm/riscv, virtio-pci on
x86-64). No kernel object / verb / dependency was added; no `cfg(target_arch)`.

### Deferred past N1a (explicit)

`udp` + `icmp` land in N1b (§7). Still deferred: `local` (the AF_UNIX-equivalent
zero-copy transport + the datapath selector), the caching `dns` client, the Linux
AF_UNIX personality (all N1c), ICMPv6, and full traceroute. See the N1 roadmap
entry above.

## 7. Phase N1b (L4 done): UDP + ICMP

### What the `net` crate adds

- **`net::udp`** - UDP datagram build/parse. `UdpHeader` parse; `build_v4`/
  `build_v6` write a full datagram; `checksum_v4`/`checksum_v6` compute the
  checksum over the **pseudo-header + UDP header + payload**; `verify_checksum_v4`
  checks a received datagram. The checksum **reuses the N1a `Checksum`
  accumulator** unchanged - the pseudo-header, header, and payload are fed as
  separate slices and the accumulator carries the odd byte across them, so the
  result matches one contiguous buffer (this is exactly why N1a built `Checksum`
  as an accumulator). A computed `0x0000` is transmitted as `0xFFFF` (RFC 768);
  IPv6 mandates the checksum (RFC 8200). An async `UdpEndpoint` (`send_to`/
  `recv_from`) frames through `wire` -> `librheo::net`, resolving the next hop via
  `arp`.
- **`net::icmp`** - ICMPv4 echo (type 8/0) build/parse with `id`/`seq`, the
  checksum via `checksum16` over the whole message (no pseudo-header), and an
  async `IcmpEndpoint` (`send_echo`/`recv_reply`/`ping`).
- **`net::wire`** - the shared L2/L3 send path both L4 protocols use:
  `frame_ipv4` (Ethernet + IPv4 header + an L4 payload, with the **TTL settable**),
  `parse_ipv4`, and `resolve_next_hop`. One place for the framing, so there is no
  per-protocol copy.

### The UDP checksum (correctness-critical)

The pseudo-header the checksum covers is `[src][dst][zero][proto][udp_len]` for
IPv4 (12 bytes) and `[src][dst][udp_len(4)][zero(3)][next_header]` for IPv6 (40
bytes). It is validated two ways: (1) a **known-good oracle** - the fixed DNS-query
datagram from `10.0.2.15:0x9876` to `10.0.2.3:53` checksums to `0x6D45`, and the
fixed ICMP echo request to `0xFFE0`, both computed independently and asserted in
the proof (like N1a's `0xB861`); and (2) the **live round trip** - `recv_from`
recomputes the checksum over each received datagram and drops any that fail.

### The TTL hook for traceroute

`wire::frame_ipv4` takes the IPv4 TTL, and both endpoints expose `set_ttl`. That
is the seam a later traceroute uses (send probes with an increasing TTL, read the
ICMP **time-exceeded** each router returns). N1b ships the hook, not the loop -
the TTL-increment traceroute + time-exceeded parsing is **N1e** (§9, done).

### The proof (`netl4` test kernel, all 3 ISAs)

A `netl4-demo` cell (loaded like `netcore`) over QEMU SLIRP + virtio-net:

1. asserts the UDP + ICMP checksum oracles (`0x6D45` / `0xFFE0`) in memory;
2. **UDP round trip** - sends a real DNS query (`A example.com`, transaction id
   `0x1234`) over UDP to SLIRP's built-in DNS responder at `10.0.2.3:53` and
   receives the reply; asserts it is from `10.0.2.3:53`, its UDP checksum
   validates (in `recv_from`), and the transaction id is echoed. DNS is **not
   parsed** (that is the N1c caching resolver) - this proves the UDP datagram
   round-tripped and its checksum is exact;
3. **ICMP echo (ping)** - sends an ICMP echo request to the gateway `10.0.2.2`
   (SLIRP answers echo to the gateway internally, no host network) and asserts the
   reply is type 0 with the matching id/seq and a valid checksum.

Bounded retransmits guard a momentary RX miss; if SLIRP does not answer, the demo
returns a nonzero code and the kernel fails loudly (no fake pass). It exits `0x42`
only if every step passes, on **all three ISAs** (virtio-mmio on arm/riscv,
virtio-pci on x86-64). No kernel object / verb / dependency was added; no
`cfg(target_arch)`.

**Why this is deterministic + network-free:** the ICMP ping targets `10.0.2.2`,
which libslirp answers itself. SLIRP's DNS at `10.0.2.3` returns a response packet
for the query regardless of the upstream result (we assert only the transaction-id
echo + a well-formed UDP reply, never the resolved address), so the proof does not
depend on real outbound DNS.

### Deferred past N1b (explicit)

`local` (the AF_UNIX-equivalent zero-copy transport + the datapath selector) and
the Linux AF_UNIX personality (the next slice). **Full traceroute** (the
TTL-increment loop + time-exceeded parsing) landed in **N1e** (§9). **Live
ICMPv6** stays deferred (the v6 Time Exceeded codec is done + unit-proven in N1e,
but SLIRP cannot generate v6 errors). The next-hop choice today ARPs the
destination directly (SLIRP proxy-ARPs `10.0.2.0/24`); a real routing table
(gateway for off-link) is a later refinement.

## 8. Phase N1c (done): the caching DNS client

### What the `net` crate adds

- **`net::dns`** - a from-scratch DNS client (no external crate):
  - **Message codec** - `build_query` (A/AAAA, a transaction id, RD set) and
    `parse_response` (header, questions skipped, answer RRs). It decodes **A**,
    **AAAA**, and **CNAME**, and follows **name-compression pointers** (the
    `0xC0` scheme - the low 6 bits + the next byte are a 14-bit offset from the
    message start).
  - **`Resolver`** - async `resolve(name, qtype) -> Result<Vec<IpAddr>, DnsError>`
    that checks, in order, the **blocklist**, the **hosts table**, and the
    **cache** (all network-free), then queries the configured resolvers over
    `udp::UdpEndpoint`, parses the reply (verifying the transaction id + source),
    and inserts the answer into the cache with the RR TTL. Bounded by a reactor
    `time::timeout` per attempt plus a retry budget.
  - **`Cache`** - an **LRU with TTL expiry** keyed on `(name, qtype)`: a lookup
    evicts an expired entry, and an insert past the cap evicts the
    least-recently-used entry. It runs on an **opaque monotonic clock** the
    caller supplies, so the math is exact and portable and the deterministic
    proof drives it directly.
  - **`Blocklist`** - exact names in a from-scratch open-addressing **hash set**
    (FNV-1a, O(1) average) plus wildcard suffixes (`*.ads.example` blocks the
    base name and every subdomain). A blocked name resolves to `Err(Blocked)`
    (or a configured **sinkhole** address) with **no network query**.
  - **`Config` + `HostsTable`** - configurable resolver IPs, an optional
    sinkhole, cache cap + query timing, and a static **hosts** table (Linux
    `/etc/hosts`-shaped) checked before the cache/network.

### Name-compression safety (correctness- and security-critical)

The classic DNS-parser bug is a crafted pointer **loop** that hangs the parser, or
a pointer past the buffer that reads out of bounds. `read_name` defends against
both: it **caps pointer jumps** at `MAX_JUMPS` (128), **rejects** any offset at or
past the message end, and **caps** the assembled name at 255 bytes. A malicious
packet gets a clean `DnsError::Parse`, never a hang. This is pinned by a
**known-good in-memory oracle** (like N1a's `0xB861` and N1b's `0x6D45`): a
hand-crafted **compressed** response (a `0xC0` pointer back to the question name)
parses to `example.com A 93.184.216.34` TTL 3600; and three crafted packets - a
self-pointer, a mutual pointer cycle, and an out-of-bounds pointer - are each
asserted to error rather than hang.

### Large blocklists (the arena path)

The `HashSet` is the O(1) shape a multi-million-entry list needs. At N1c test
scale its slots + interned names live on the general heap; for a truly huge list
they would live in a **grant-backed `librheo::mem` arena** (a reserved, committed
typed grant) instead - the documented path, not built here.

### The proof (`netdns` test kernel, all 3 ISAs)

A `netdns-demo` cell (loaded like `netl4`) over QEMU SLIRP + virtio-net. The
**core assertions are deterministic and network-free** - they hold with no
outbound internet, so they are the proof:

1. **Parse oracle** - the compressed response decodes to the exact A record; the
   three crafted pointer-loop / out-of-bounds packets all error (no hang).
2. **Hosts table** - names in the static hosts table resolve to their configured
   IP with the query counter at **zero**.
3. **Blocklist** - an exact-blocked name and a wildcard-blocked name both return
   `Err(Blocked)` with the counter at **zero**.
4. **Cache hit** - a pre-seeded name resolves from cache (twice, case/trailing-dot
   normalized to the same key) with the counter at **zero**; a standalone `Cache`
   unit proves TTL expiry + LRU eviction on an explicit clock.

The **query counter** (`Resolver::queries_sent`) is the network-free evidence: a
blocklist / hosts / cache hit sends nothing, so `queries_sent == 0` proves the
short-circuit. Then a **bonus live** resolve of `example.com` over SLIRP's DNS
(`10.0.2.3:53`) asserts only **structure** - a valid A record present, a query was
sent, and a second lookup is a cache hit (no extra query) - never a specific
address (SLIRP proxies to the host resolver, so the address is non-deterministic).
If this sandbox has **no outbound DNS**, the resolve times out cleanly and is
**tolerated** (the deterministic checks already passed); the cell never fakes a
pass. It exits `0x42` only if every deterministic check passes, on all three ISAs.
No kernel object / verb / dependency was added; no `cfg(target_arch)`.

### Deferred past N1c (explicit)

**Negative caching** (caching an NXDOMAIN for a short TTL) is deferred - each
NXDOMAIN currently re-queries; the seam is `Config` + `Cache`. The codec + resolver
support **AAAA**; only the *live* proof is A (SLIRP proxies to the host resolver, so
a deterministic AAAA answer is not guaranteed). `local`/AF_UNIX (the zero-copy local
path + the Linux AF_UNIX personality) is the next N1 slice.

## 9. Phase N1e (done): first-class TTL / hop limit + traceroute

N1b shipped only a settable-TTL *hook* and deferred the rest. N1e makes TTL
(IPv4) and Hop Limit (IPv6) **first-class and correct**, adds ICMP/ICMPv6 Time
Exceeded, a real traceroute state machine, and the forwarding-plane decrement
primitive (the router/firewall path).

### First-class TTL / hop limit (`net::ip`)

- `ip::DEFAULT_TTL = 64` and `ip::DEFAULT_HOP_LIMIT = 64` (RFC 1122 §3.2.1.7 for
  v4, RFC 8200 §3 for v6). The IPv4 TTL and the IPv6 hop limit are the **same
  concept** - the number of forwarding hops a datagram may still cross, one byte
  each - so rheo-net treats them symmetrically. Both fields are built **and**
  parsed and round-trip (the N1e proof asserts the default and an explicit value
  for each).
- **The forwarding-plane decrement primitive** (the router/firewall forward
  path). `ip::decrement_ttl(hdr: &mut [u8]) -> Option<()>` operates on an on-wire
  IPv4 header: a node **forwarding** a datagram runs it per hop. It returns `None`
  when the TTL is already `0` or `1` (the datagram **expires here** - the caller
  drops it and emits a Time Exceeded, because forwarding a TTL-1 datagram would
  make it 0, which RFC 791 forbids), and on a real forward it decrements the TTL
  and **recomputes the IPv4 header checksum**. `ip::decrement_hop_limit` mirrors
  it for IPv6 (no checksum to recompute - RFC 8200 dropped it).

**Checksum on decrement (correctness-critical).** The decrement recomputes the
header checksum by a **full recompute** via the N1a `checksum16` scalar oracle
(zero the field, sum the header, store) - not the RFC-1624 incremental update.
Full recompute is chosen for clarity and because it reuses the one audited
checksum routine (no second code path to keep correct). It is pinned by a
**known-good oracle**: the netcore RFC example header (TTL `0x40`, checksum
`0xB861`) after `decrement_ttl` (TTL `0x3F`) checksums to `0xB961`, and the
resulting header re-verifies (`verify_checksum` folds to zero) - both asserted.

### ICMP Time Exceeded (`net::icmp`)

- **ICMPv4 Time Exceeded (type 11, code 0)**: `build_time_exceeded` / `parse_error`
  build and parse the message with the standard payload - the offending IP header
  **plus its first 8 bytes** (RFC 792). Those 8 bytes are what a traceroute
  correlates on. Pinned by a known-good oracle: a fixed Time Exceeded checksums to
  `0xF4FF`, self-verifies, and round-trips its embedded original.
- **ICMPv6 Time Exceeded (type 3, code 0)**: `build_time_exceeded_v6` /
  `verify_checksum_v6` with the IPv6 **pseudo-header** checksum (via the existing
  `Checksum` accumulator, next-header 58). Pinned to `0x1936`. The v6 **codec** is
  done + unit-proven; the *live* v6 path is deferred (SLIRP cannot generate v6
  ICMP errors - see the proof split below).

### The traceroute state machine (`net::trace`)

**Probe method (documented choice):** an **ICMP echo request whose sequence
number equals the TTL** (the scheme Windows `tracert` uses). RFC 792 guarantees a
Time Exceeded echoes back the offending IP header + its first 8 bytes; for an
echo probe those 8 bytes are the echo header, so the **sequence number (= the hop)
survives inside the router's reply** - a correlation key with no extra state.
(Classic Unix traceroute uses UDP to a per-hop port; the echo scheme keeps send +
correlate in one module and reuses `icmp::IcmpEndpoint`.)

`trace::Tracer` is a **pure state machine with no I/O**: it hands out the next
probe TTL (from 1 upward, bounded at `max_hops`, default 30), consumes
already-correlated `Response`s (`trace::classify` turns a received ICMP message
into a `TimeExceeded { seq, from }` or `Reply { seq, from }`), records each hop,
and terminates on the destination's Echo Reply. A per-hop timeout is recorded as a
silent hop (`0.0.0.0`) so one unresponsive router does not stall the trace.
`Tracer::run` is the thin live driver over an `IcmpEndpoint` (set TTL, send probe,
`recv_trace`, record), retried per `attempts`.

### The proof (`nettrace` test kernel, all 3 ISAs)

A `nettrace-demo` cell (loaded like `netdns`) over QEMU SLIRP + virtio-net. The
**deterministic core is the proof** (network-free):

1. **TTL / hop-limit round-trip** - an IPv4 header round-trips its TTL (default 64
   + explicit) and an IPv6 header its hop limit (default 64 + explicit).
2. **The decrement primitive** - `decrement_ttl` from `N -> N-1` recomputes a
   valid checksum matching the `0xB961` oracle, and from TTL `1` (and `0`) returns
   the drop signal (`None`); `decrement_hop_limit` mirrors it for v6.
3. **Time Exceeded oracles** - the ICMPv4 Time Exceeded checksums to `0xF4FF`,
   self-verifies, and round-trips its embedded original; the ICMPv6 codec
   checksums to `0x1936` and self-verifies.
4. **The traceroute state machine fed synthetic responses** - a crafted sequence
   of Time Exceededs (hops 1..3, distinct router IPs) then a destination Echo
   Reply is put through the real `build_time_exceeded` -> `classify` ->
   `Tracer::record` path, and the tracer reconstructs the **exact ordered 4-hop
   list** (3 routers + destination) and terminates. This proves **multi-hop
   discovery without real intermediate routers**.

Then a **bonus live** 1-hop trace to the gateway `10.0.2.2` over SLIRP. **SLIRP
has no intermediate hops** - it is the destination at hop 1 and answers the echo
directly - so the demo asserts only that (a reached hop is `10.0.2.2` at TTL 1)
and **tolerates a clean timeout** with a printed reason, never faking a pass.
Multi-hop discovery is proven by the parser + state machine (step 4), **not** by
the emulator: SLIRP cannot generate intermediate Time Exceededs, and it cannot
generate ICMPv6 errors at all (hence the live v6 deferral). The cell exits `0x42`
only if steps 1-4 pass, on **all three ISAs**. No kernel object / verb /
dependency was added; no `cfg(target_arch)`.

### Deferred past N1e (explicit)

The **live ICMPv6** traceroute (the v6 codec is done + unit-proven; a live proof
needs a v6 backend SLIRP does not provide). A full IPv6 **send** framing path
(`wire` frames IPv4 today; the v6 hop-limit field is first-class and round-trips,
but a v6 `Ipv6Framing` + v6 endpoints ride in with the live v6 path). PMTU
discovery and IGMP/MLD multicast stay in **N7**. The forwarding decrement is the
primitive a router/firewall runs; wiring it into an actual multi-cell forwarding
service is a later phase (the mechanism is the N1e deliverable).

## 10. Phase N1d (done): local sockets - native `net::local` + Linux AF_UNIX

The local fast path (the "skip parts of the stack" mechanism of §2): a connection
selects its datapath at connect time - `local` (zero-copy cell-to-cell IPC, no IP)
vs `wire` (the full stack). N1d ships the working `local` path two ways - a native
API for rheo-native cells and Linux-personality **AF_UNIX** for unmodified Linux
binaries - and the datapath **selector** itself. It adds **no kernel object** and
**no `cfg(target_arch)`** (the socket *numbers* live in `arch/*/linux_abi`, which
is allowed per-ISA ABI, and are the only per-ISA part).

### (A) Native `net::local`

- **`net::local`** (`net/src/local.rs`) - a thin typed API over
  `librheo::ipc::Channel` (a shared cross-cell queue pair) + sealed-grant buffer
  passing (the dmabuf equivalent). `LocalStream::connect`/`accept` open the two
  ends; `send`/`await_completion`/`recv`/`complete` carry inline messages;
  `share(grant)` delegates a **sealed** grant zero-copy and `recv_buffer(peer_va,
  len)` views the *same frames* on the peer. No IP, no Ethernet, no copy.
- **The datapath selector** - `local::select(&Target) -> Datapath`: a
  `Target::Local` (same-host peer) chooses `Datapath::Local` (zero-copy IPC), a
  `Target::Remote(IpAddr)` chooses `Datapath::Wire` (the IP stack). For N1d the
  wire side is a stub (a wire connect is a later phase); the selection + the
  working local path are the deliverable.
- **Proof (`netlocal` test kernel, all 3 ISAs)** - ONE binary (`netlocal-demo`)
  run as **two cells** sharing a channel (mirroring `librheowl`), needing **no
  netdev**. The client checks the selector (local->Local, remote->Wire), draws a
  known 4 KiB payload into a buffer grant, seals + `share`s it, and hands the peer
  VA + length over the local stream; the server maps the shared grant read-only
  (the SAME frames), checksums it, and replies. The client asserts the server's
  checksum equals its own (proving zero-copy) and exits `0x42`.

### (B) Linux AF_UNIX (the L8 start)

AF_UNIX in the Linux personality (docs/LINUX-COMPAT.md L8): sockets are per-cell
fds (`kernel/src/linux/fd.rs`), the byte transport is the **L6 cross-cell ring**
(`kernel/src/linux/pipe.rs`) - a SOCK_STREAM connection is two rings, one per
direction - and the only new global state is a **name registry + accept queue**
(`kernel/src/linux/unixsock.rs`), per-personality synthesized state exactly like
the L6 pipe table (no kernel object; the L6 `pipe2` set the precedent).
`socket`/`socketpair`/`bind`/`listen`/`accept`/`accept4`/`connect`/`getsockname`/
`sendto`/`recvfrom`/`sendmsg`/`recvmsg`/`setsockopt`/`getsockopt`/`shutdown` are
wired into all three `arch/*/linux_abi` tables. Abstract-namespace names
(`\0`-prefixed) are supported. **Proof (`linuxunix`, all 3 ISAs, exact stdout +
exit)**: an unmodified static-glibc C fixture (`af_unix.c`, built from source by
xtask, never committed) does `socketpair(AF_UNIX, SOCK_STREAM)` + `fork` (parent
and child ping/pong over the two rings) and `socket`/`bind`/`listen`/`connect`/
`accept` over an abstract name (a loopback hello/world), printing a fixed
transcript and exiting 0.

### Deferred past N1d (explicit)

**SCM_RIGHTS fd-passing** is deferred (the seam is `sendmsg`'s `msg_control`; it
is not faked). **AF_UNIX SOCK_DGRAM** is refused (`-EPROTONOSUPPORT`) - datagram
boundary preservation is not implemented for the Unix domain. **`accept` is
non-blocking** (the loopback proof connects before accepting); a blocking
cross-cell accept server is a later refinement. `getpeername` reports family-only
for an unnamed AF_UNIX peer.

### (C) Linux AF_INET / AF_INET6 loopback (the L8-INET slice)

The socket surface extends from AF_UNIX to the **internet domain** so *unmodified
networked Linux binaries run* (docs/LINUX-COMPAT.md L8-INET). Architecture
decision, forced by doctrine: the kernel is **allocation-free**, so the native
`net::tcp`/`net::udp` (`no_std`+**alloc** userspace crates) **cannot** be linked
kernel-resident. For the **loopback** interface (127.0.0.1 / ::1) a TCP connection
between two local endpoints reduces to a **reliable, in-order byte stream** -
precisely the L6 ring pair that already backs AF_UNIX SOCK_STREAM - and UDP to an
in-order **datagram queue**. So AF_INET/AF_INET6 sockets run over loopback
in-personality, deterministic and network-free (`kernel/src/linux/inetsock.rs`,
keyed by `(is_v6, port)`), adding **no kernel object**. This proves the socket
**ABI**; **NIC-backed remote INET** - driving the full `net::tcp` segment/RTO/
congestion state machine (§11) over the virtio-net raw-frame path - is a **named
later phase** (a non-loopback destination is refused `-ENETUNREACH`). Landed:
`socket(AF_INET|AF_INET6, SOCK_STREAM|SOCK_DGRAM)`, `bind`/`listen`/`accept`/
`connect`, stream `read`/`write`/`send`/`recv` (over the L6 block+SIGPIPE path),
`sendto`/`recvfrom` (loopback datagrams with source-address reporting), real
`sockaddr_in`/`sockaddr_in6` `getsockname`/`getpeername`, no-op
`setsockopt`/`getsockopt`, and a minimal **level-triggered epoll**
(`epoll_create1`/`epoll_ctl`/`epoll_wait`/`epoll_pwait`, `EPOLLIN`/`EPOLLOUT`,
`kernel/src/linux/epoll.rs`). **Proof (`linuxinet`, all 3 ISAs, exact stdout +
exit)**: an unmodified static-glibc C fixture (`inet.c`) does a TCP client/server
+ epoll readiness + UDP over 127.0.0.1 and a TCP exchange over ::1. Deferred:
NIC-backed remote INET (above), edge-triggered/oneshot epoll, blocking
`epoll_wait`, IPV4_MAPPED dual-stack, and effectful socket options.

## 11. Phase N2a (done): the native TCP core + a timer wheel

TCP is the meat of the transport layer. N2a builds the **state machine** and the
**timer wheel** it needs, over `net::ip` + the N1a `Checksum` accumulator (the TCP
checksum uses the same pseudo-header shape as UDP) - portable userspace, **no
kernel object**, **no reactor/ABI change**, **no new dependency**, **no
`cfg(target_arch)`**. Congestion control, the smoltcp cell, and the sharded
transport are **N2b**; N2a wires the CC **seam** so N2b is a drop-in.

### The state machine (`net::tcp`)

- **`Connection<C>`** (`net/src/tcp.rs`) is a **poll-driven, synchronous,
  deterministic** state machine - no I/O and no async inside (the smoltcp lesson:
  an ambient stack is unprovable). A driver feeds it received segments
  (`on_segment` / `on_wire_segment`, the latter decoding + **verifying the TCP
  checksum** first) and a monotonic `now` (nanoseconds), and pulls the next segment
  to transmit from `poll(now)`; `poll_at()` reports when it next needs attention
  (its RTO / TIME-WAIT deadline). This is exactly what makes it provable **without a
  live peer**.
- **Full state set** (RFC 793): CLOSED / LISTEN / SYN_SENT / SYN_RCVD / ESTABLISHED
  / FIN_WAIT_1 / FIN_WAIT_2 / CLOSING / CLOSE_WAIT / LAST_ACK / TIME_WAIT. The
  three-way handshake (active `connect` + passive `listen`), FIN teardown (active,
  passive, and simultaneous close), and the `2*MSL` TIME-WAIT dwell.
- **Sliding window + flow control**: a send queue with `snd_una`/`snd_nxt`, the
  effective send window = `min(peer_advertised_window, cwnd)`, MSS-bounded
  segmentation, cumulative-ACK processing that drops acked bytes, and an advertised
  receive window = free receive-buffer space. In-order receive delivery.
- **The TCP checksum** (`checksum_v4` / `verify_checksum_v4`) reuses the N1a
  `Checksum` accumulator with the `src, dst, zero, proto=6, tcp_len` pseudo-header
  and the segment (checksum field zeroed for compute, in place for verify).

### Sequence-number arithmetic (correctness-critical)

Sequence numbers are 32-bit and **wrap**; every window/ack comparison goes through
the RFC 1323 serial-number helpers (`tcp::seq`): `a` is before `b` iff `(a - b)` as
a signed 32-bit value is negative. This lives in one place with a **wrap oracle**
(the proof asserts `0xFFFF_FFFF < 0`) - getting it wrong is the classic TCP bug.

### RTO / RTT (RFC 6298) + Karn's algorithm

The retransmission timeout is estimated per RFC 6298: a smoothed RTT (`SRTT`) and
its variance (`RTTVAR`), `RTO = SRTT + max(G, 4*RTTVAR)` clamped to
`[RTO_MIN, RTO_MAX]`. **Karn's algorithm**: an RTT sample is never taken from a
retransmitted segment (the ack is ambiguous), and the RTO **backs off
exponentially** (doubles, capped) on each timeout until a fresh ack re-measures it.
Unacked data is retransmitted from `snd_una` when the RTO fires.

### The congestion-control seam (N2b slots in here)

`trait CongestionControl { on_ack(bytes, rtt); on_loss(); cwnd(); }` is the seam.
N2a ships only **`FixedWindow`** (a large fixed cwnd, so the peer's advertised
window - flow control - dominates). **N2b** slots in real controllers (Reno, CUBIC)
as drop-in `impl CongestionControl`s (§12); `Connection` is generic over `C` so
swapping the controller is a type parameter, not a rewrite.

### The socket-shaped API

`TcpStream` (`connect`/`read`/`write`/`close`) and `TcpListener` (`bind`/`accept`)
are the socket vocabulary over `Connection`; the segment transport - the in-cell
`VirtualLink` in the proof, a wire link in N2b - is driven by the owner via
`poll`/`on_wire_segment`.

### The timer wheel (`net::timer`) over the single reactor slot

The reactor exposes **one** `timer_req` slot (`rt::sleep_ns` over `SYS_ARM_TIMER`),
but TCP needs several concurrent timers (per-connection RTO, TIME-WAIT, and -
deferred in N2a - delayed-ACK, keepalive). `TimerWheel` multiplexes them: a
`BTreeSet<(deadline, id)>` (ordered nearest-deadline) + a `BTreeMap<id, deadline>`
(cancel/re-arm by id), all `O(log n)` - the **simple sorted-set** variant, not a
hashed/hierarchical wheel (that is the N2b optimization once the sharded transport
drives thousands of connections). The reactor's one-shot is **relative** (it fires
after a *duration*, and a cell has no userspace ticks->ns reading), so the wheel
owns a monotonic `now_ns`: deadlines are absolute in that frame, and `run_once`
sleeps the **delta** `(nearest - now)` on the single slot then advances `now` and
expires. Only the nearest deadline is ever armed; firing it re-arms for the new
nearest. **No kernel or reactor ABI change** - pure userspace bookkeeping over the
existing slot.

### The proof (`nettcp` test kernel, all 3 ISAs)

A cell (`nettcp-demo`) runs **two TCP endpoints in one cell** connected by an
in-cell `VirtualLink` (each endpoint's output segments are fed to the other's
input), driven through the full lifecycle by a logical clock the `TimerWheel`
advances - **deterministic and network-free** (no NIC, no SLIRP, no live peer, the
same philosophy as the traceroute/DNS deterministic proofs). It asserts, exiting
`0x42` only if every step passes:

1. **Checksum + segment-encode oracles** (in memory): a fixed SYN-ACK-with-MSS
   segment encodes to a known-good byte string and checksums (over the IPv4
   pseudo-header) to the independently computed `0x613C`, decodes back, and
   self-verifies; plus the RFC 1323 wrap oracle.
2. **The full lifecycle**: the three-way handshake completes (both ESTABLISHED); a
   known payload transfers **both directions** with the received bytes **exactly
   equal** to the sent bytes; a **dropped data segment is retransmitted after the
   RTO** and delivered (the link drops one segment, the wheel advances the clock
   past the RTO, recovery is asserted - and the link dropped exactly once); and a
   clean FIN/FIN-ACK teardown reaches CLOSE_WAIT -> TIME_WAIT -> CLOSED.
3. **The socket-shaped API**: `TcpStream`/`TcpListener` drive a second full
   handshake + byte + close.
4. **The timer-wheel multiplex**: four timers armed out of order fire in **deadline
   order** off the reactor's single one-shot (`run_once` behind `rt::sleep_ns`).

**Live path (honest):** a live TCP handshake to SLIRP was **skipped with reason** -
SLIRP has no built-in TCP echo/responder, so there is no deterministic live peer to
handshake against (unlike the ARP/DNS/ICMP proofs, where SLIRP answers). Faking a
live connection would be dishonest; the in-cell loopback lifecycle is the real
proof. A live TCP echo / HTTP GET is an **N2b** deliverable (over a real service or
an external peer at the hardware lab).

### What N2a simplifies / defers (honest)

- **No SACK, no window scaling, no timestamps option** - only the MSS option is
  emitted/parsed. Large bandwidth-delay products and selective repair are N2b.
- **No out-of-order reassembly**: an out-of-order segment is dropped and the
  receiver re-acks `rcv_nxt`, relying on retransmission (correct, not optimal).
- **Immediate ACKs** (no delayed-ACK timer) and **no keepalive**; the wheel
  supports both as more logical timers - wired in a later slice.
- **Zero-window** handling is minimal: a zero advertised window stalls the sender
  until a window update arrives (no persist-timer probe yet).
- **RST handling is minimal**: a received RST drops the connection to CLOSED.
- **Congestion control itself** (only the `FixedWindow` seam ships) and **ECN** are
  N2b (§12).

## 12. Phase N2b (done): TCP congestion control - Reno + CUBIC

N2b fills the N2a seam with two **from-scratch** congestion controllers over the
`CongestionControl` trait - portable userspace, **no kernel object**, **no ABI
change**, **no new dependency**, **no `cfg(target_arch)`**. Both use **integer /
fixed-point** cwnd math (matching the kernel's no-FPU discipline even though these
are soft-float U-mode cells; no float appears in the window arithmetic).

### The controllers (`net::cc`)

- **`Reno`** (RFC 5681), all in bytes:
  - **Slow start** (`cwnd < ssthresh`): `cwnd += bytes_acked` per ACK - exponential,
    one doubling per RTT.
  - **Congestion avoidance** (`cwnd >= ssthresh`): a byte accumulator adds one MSS
    per cwnd worth of acked bytes - the clean, coalescing-robust form of AIMD's
    `cwnd += MSS*MSS/cwnd` (linear, +1 MSS/RTT).
  - **Fast retransmit / fast recovery** (3rd duplicate ACK): `ssthresh =
    max(cwnd/2, 2*MSS)`, `cwnd = ssthresh + 3*MSS` (inflate); each further dup ACK
    inflates by one MSS; the first new ACK deflates to `cwnd = ssthresh` and exits
    recovery.
  - **RTO** (`on_loss`): `ssthresh = max(cwnd/2, 2*MSS)`, `cwnd = 1*MSS` (slow-start
    restart).
- **`Cubic`** (RFC 8312): the window follows `W(t) = C*(t - K)^3 + W_max` with
  `beta = 0.7`, `C = 0.4`, `K = cbrt(W_max*(1-beta)/C)` - **concave** approaching the
  pre-loss `W_max`, **convex** past it - guarded below by the TCP-friendly estimate
  `W_est(t) = W_max*beta + (3*(1-beta)/(1+beta))*t/RTT` (`cwnd = max(W_cubic, W_est)`).

### The CUBIC fixed-point scheme (correctness-critical)

No floats. Windows are in **bytes**, the cubic term's time in **milliseconds**. With
`C = 0.4 = 2/5`, `beta = 0.7 = 7/10`, `1-beta = 3/10`, `3*(1-beta)/(1+beta) = 9/17`:
- `K` (ms) `= cbrt(W_max_segments * (1-beta)/C * 1e9) = cbrt(3 * W_max_seg *
  250_000_000)`, via an **integer cube root** (`icbrt`, a binary search).
- the cubic term (bytes) `= MSS * C * (dt_ms/1000)^3 = (2 * MSS * dt_ms^3) /
  5_000_000_000`, computed in `i128` so `dt_ms^3` never overflows and the sign of a
  pre-`K` (`dt < 0`) term is exact (Rust truncates toward zero - the oracle is
  computed the same way).
- `W_est` base `= (7*W_max)/10`, slope-per-RTT `= (9*MSS)/17`.

This tracks the real-valued cubic to within a few bytes across the sampled
timescale (measured max deviation 8 bytes at `W_max = 32 MSS`).

### How they wire to the send window

`Connection<C>` is generic over the controller; the usable send window stays
`min(peer_advertised_window, cwnd())`, unchanged from N2a. The connection now:
- calls `cc.tick(now)` before every ack/loss step so a **time-based** controller
  (CUBIC) can evaluate `W(t)` (ack-clocked controllers - Reno, `FixedWindow` -
  ignore it);
- **detects duplicate ACKs** (RFC 5681: a pure ACK that doesn't advance `snd_una`,
  carries no data, and leaves the window unchanged while data is outstanding) and
  calls `cc.on_dup_ack()`; on the 3rd it sets a `fast_retransmit` flag, and the next
  `poll` rewinds `snd_nxt` to `snd_una` to **retransmit the lost segment immediately,
  before the RTO** (no RTO backoff, distinct from the timeout path);
- feeds `cc.on_ack(bytes_acked, rtt)` / `cc.on_loss()` from the existing ack and RTO
  paths.

The trait grew `tick` / `on_dup_ack` / `ssthresh` / `in_recovery` / `set_mss`, all
**default-implemented**, so `FixedWindow` and the whole N2a `nettcp` proof are
byte-for-byte unchanged.

### The proof (`nettcpcc` test kernel, all 3 ISAs)

A cell (`nettcpcc-demo`) drives **deterministic integer cwnd trajectories**, each
pinned against a precomputed oracle, exiting `0x42` only if every one matches
(entirely in-cell - no NIC, no SLIRP, the N2a deterministic philosophy):

1. **Reno slow start**: `cwnd` doubles per ACK round, `1 -> 2 -> 4 -> 8 -> 16 MSS`,
   up to `ssthresh`.
2. **Reno AIMD**: past `ssthresh`, `cwnd` grows by exactly one MSS per round (linear).
3. **Reno fast retransmit / recovery** (scripted dup ACKs): the 3rd dup returns the
   trigger; `ssthresh = 8 MSS` (`cwnd/2`), `cwnd = 11 MSS` (`ssthresh + 3*MSS`); two
   further dups inflate to `12`, `13 MSS`; the first new ACK deflates to `8 MSS`
   (**not** a collapse to 1 MSS).
4. **Reno RTO collapse**: `ssthresh = 8 MSS`, `cwnd = 1 MSS`.
5. **CUBIC shape**: after a loss with `W_max = 32 MSS`, `W(t)` at seven sampled
   times matches the integer oracle `[32712, 42815, 46317, 46720, 47531, 52252,
   64388]` bytes (K ~= 2.884 s, where `W == W_max = 46720`) **and** stays within 16
   bytes of the real-valued cubic; the increments are concave (decreasing:
   `10103, 3502, 403`) before `K` and convex (increasing: `811, 4721, 12136`) after.
6. **CUBIC vs Reno**: from the same pre-loss `32 MSS`, at a matched late checkpoint
   CUBIC (`~44 MSS`, convex growth from a gentler `0.7` decrease) exceeds Reno
   (`22 MSS`, linear growth from a `0.5` decrease).
7. **Integration** - a real `Connection<Reno>` over the in-cell virtual link: a
   held-back segment makes the receiver re-ACK, three duplicate ACKs
   **fast-retransmit the lost segment before the RTO deadline**, `cwnd` halves (fast
   recovery, `ssthresh = 4 MSS`, **not** RTO collapse), and the full payload still
   transfers (received == sent).
8. **Bulk transfer**: a real `Connection<Reno>` and `Connection<Cubic>` each carry a
   20-segment payload to completion (received == sent) with `cwnd` grown from slow
   start.

**Live path (honest):** a live TCP handshake to SLIRP is **skipped with reason** (as
in N2a): SLIRP's user-net has no TCP echo/responder, so there is no deterministic
live peer, and faking one would be dishonest. A live TCP echo / HTTP GET is an
**N2c / hardware-lab** deliverable. The deterministic cwnd-trajectory proof is the
real deliverable.

### What N2b simplifies / defers (honest)

- **Reno, not full NewReno**: fast recovery exits on the first new ACK; NewReno
  **partial-ACK** deflation (staying in recovery across multiple losses in a window)
  is deferred - it wants SACK, also deferred.
- **CUBIC is time-clocked, not ack-clocked**: `on_ack` sets `cwnd` to `W(t)` for the
  elapsed `t`, rather than the incremental `cwnd += (W(t+RTT) - cwnd)/cwnd` per ACK.
  Same trajectory, cleaner to pin against an oracle; the per-ack increment is the
  optimization.
- **CUBIC HyStart and fast-convergence are deferred** (documented, not built):
  pre-loss CUBIC stays in slow start until a loss, and a loss always saves
  `W_max = cwnd` (no fast-convergence discount).
- **BBR** and **ECN** are later phases.
- The two transports (**smoltcp** blessed cell + the **native sharded** transport)
  are **N2c**, as is a **live** TCP handshake.

## 13. Phase N2c (done): the two transports - smoltcp blessed cell + native sharded framing

N2c delivers the two transports the design has named since §3: the **blessed
correctness-first** stack (smoltcp) for control/low-rate cells, and the **native
sharded** framing for the HFT/warehouse hot lines. Both ride the existing
raw-frame NIC path - **no kernel object, no verb, no ABI/reactor change, no
`cfg(target_arch)`**. The from-scratch `net::{tcp,cc,udp,ip,eth,...}` stack and
every pre-existing test are **unaffected**: smoltcp sits *alongside* it behind a
cargo feature, never replacing it.

### (A) The smoltcp blessed transport cell (`net::smoltcp_cell`, feature `smoltcp`)

**smoltcp** is the doc-named blessed pure-Rust `no_std` transport (§3, Redox's
stack, correctness-first, single-poll, no ambient threads). N2c integrates it as
an **alternative** transport running in a loaded rheo-os cell **over the raw-frame
NIC path** (`librheo::net`: `OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC` -> virtio-net).

- **Dependency (the one N2c adds, per the no-deps rule).** `smoltcp = "=0.13.1"`,
  pinned, `default-features = false`, features `medium-ethernet` / `proto-ipv4` /
  `proto-ipv6` / `socket-udp` / `socket-tcp` / `alloc`. It builds `no_std` for all
  three bare targets (`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`,
  `riscv64gc-unknown-none-elf`) - **no transitive dep pulls `std`**. Its small tree
  (`bitflags`/`byteorder`/`cfg-if`/`heapless`/`managed`/`hash32`) is all `no_std`;
  `defmt`/`log` are optional and left off. It is behind the **`smoltcp` cargo
  feature** (off by default) so nothing links it unless a cell opts in - the
  `netsmoltcp-demo` bin sets `required-features = ["smoltcp"]` and is built by a
  dedicated xtask step, so `build_userland` (default features) never touches it.
- **The `phy::Device` bridge (the load-bearing integration).** smoltcp's
  `phy::Device` is **synchronous** (`receive`/`transmit` pop/push frames with no
  `.await`), while `librheo::net::send`/`recv` are **async** over the strand
  reactor. `QueueDevice` bridges them the standard async-over-smoltcp way: two
  `VecDeque`s. The async driver (`smoltcp_cell::pump`) pulls frames off the NIC
  with `net::recv` into the device's RX queue, the caller runs smoltcp's
  synchronous `iface.poll`, then the driver ships the device's TX queue out with
  `net::send`. So the `RxToken`/`TxToken` consume/produce exactly the frames
  `net::recv`/`net::send` carry - smoltcp drives the real virtio-net driver end to
  end, one hop removed by the queue buffer.
- **The clock.** smoltcp wants a monotonic millisecond `Instant`. A cell has **no
  userspace ticks->ns reading** (the kernel owns the timebase; `librheo::time`
  documents the gap), so the driver advances smoltcp's clock by the **real**
  duration it sleeps between polls (`librheo::time::sleep`, a genuine kernel
  one-shot deadline): sleep 2 ms, advance the smoltcp clock 2 ms. The clock is
  therefore real monotonic milliseconds, not a synthetic counter - honest.
- **The interface.** `smoltcp::iface::Interface` with an Ethernet `Config` (the
  NIC MAC), the SLIRP guest IP `10.0.2.15/24`, over the `QueueDevice`.

### (B) The native sharded transport framing (`net::shard`)

The Snap/Seastar **shared-nothing** shape: `shard::Transport` owns N `Shard`s,
each holding a **disjoint** set of connections in its own `BTreeMap`;
`connect`/`listen` route to the owning shard by hashing the connection's
`FourTuple` (FNV-1a over the canonical 12 bytes, the same from-scratch idiom
`net::dns`'s blocklist uses; a per-epoch keyed seed - §8.3 - is a later
refinement). There is **no shared mutable stack state** between shards, so a flood
or a bug on one shard's connections cannot reach another's (the §1 DDoS-isolation
story).

**Honest under the single-CPU cooperative model (docs/CONCURRENCY.md; SMP is task
#27):** the shards **interleave on one core** - this is *structural* isolation
(disjoint ownership, no cross-shard aliasing), **NOT** parallel throughput. A
truly parallel per-core transport - each shard pinned to its own hart/vcore,
connections steered by hardware RSS - awaits SMP. N2c delivers the *framing* (the
hash-to-shard routing + the shared-nothing ownership discipline), so the parallel
version is a scheduling change, not a rewrite.

*(Note: with N shards a power of two, `FourTuple::hash % N` and the mirror-tuple
relation interact - FNV-1a's **low bit is order-independent**, so a tuple and its
mirror always share a shard under `% 2`. The N2c proof therefore demonstrates
per-shard function by driving each shard-owned connection against a **locally-held
peer** (the remote host - not another shard), which is the realistic case: a
transport owns only its local ends.)*

### The proof (`netsmoltcp` test kernel, all 3 ISAs)

A `netsmoltcp-demo` cell (loaded like `netl4`, over QEMU SLIRP + virtio-net;
virtio-mmio on arm/riscv, virtio-pci on x86-64) proves, exiting `0x42` only if
every step passes:

1. **(B) Native sharded transport (deterministic, network-free):** a
   `shard::Transport` of **2 shards** routes a set of 32 connections; the set
   **partitions** across both shards (each connection owned by exactly the shard
   its hash names, in no other - shared-nothing), and a shard-0-owned **and** a
   shard-1-owned connection each complete a full TCP handshake + byte transfer
   over the in-cell `VirtualLink` (reusing the N2a machinery) against a
   locally-held peer.
2. **(A1) smoltcp in-cell over `Loopback` (deterministic, network-free):** a
   smoltcp TCP client + server over smoltcp's built-in `Loopback` device complete
   a handshake and transfer bytes; a smoltcp UDP socket pair round-trips a
   datagram. This pins the integration (Device trait, Interface, SocketSet, the
   poll loop, `alloc`, the ms clock) with no network.
3. **(A2) smoltcp live UDP over the real NIC:** a smoltcp UDP socket sends a DNS
   query to SLIRP's built-in responder `10.0.2.3:53` over the `QueueDevice` bound
   to `librheo::net`, and **receives the reply** (asserted: from `10.0.2.3:53`,
   transaction id echoed) - proving smoltcp drives our virtio-net driver end to
   end. Deterministic like `netl4`'s live UDP (SLIRP answers regardless of the
   upstream result). If a future sandbox has no SLIRP DNS, this step's timeout is
   the honest failure signal; steps 1-2 are the network-free core.

No kernel object / verb / dependency-in-the-kernel was added; smoltcp is the one
doc-named userspace dependency (pinned, `no_std`, feature-gated); no
`cfg(target_arch)`.

### What N2c defers (honest)

- **smoltcp TCP over the live NIC** (a real remote TCP peer): SLIRP has no
  built-in TCP echo/responder (the N2a/N2b deferral), so the live smoltcp proof is
  UDP (which SLIRP answers via its DNS); the in-cell `Loopback` TCP is the
  deterministic TCP proof. A live TCP echo / HTTP GET is a hardware-lab / N5
  (HTTP) deliverable.
- **Zero-copy in the smoltcp path:** the `QueueDevice` copies each frame into/out
  of its `VecDeque` (smoltcp's tokens want owned buffers). smoltcp is the
  *correctness-first* transport (§3) - zero-copy DMA to the wire is the native
  sharded transport's job and the N6 perf substrate; smoltcp is deliberately not
  the hot-line stack.
- **Parallel sharding:** structural only under the single CPU (above) - SMP (#27).
- The per-epoch **keyed** shard hash (§8.3) and a cross-shard queue-pair steering
  path are later-phase refinements; N2c ships the deterministic FNV framing.

## 14. Phase N3a (done): the crypto primitive layer

N3a establishes the crypto foundation the security transports (TLS 1.3 /
WireGuard / IPsec, N3b+) build on, per the §3 **hybrid** posture. It adds **no
kernel object, no verb, no kernel change** (crypto is pure userspace), **no
`cfg(target_arch)`** (portable Rust; the crates handle their own arch internally),
and the crypto crates are **doc-named dependencies** (§3, pinned, `no_std`,
feature-gated behind the `net` crate's `crypto` feature so the base stack + every
existing test are unaffected). The TLS handshake itself is **N3b** - out of scope
here; N3a is the vetted primitives.

### What the `net` crate adds (`net::crypto`, feature `crypto`)

- **`chacha`** - ChaCha20 block + `xor_keystream` + `poly1305_key` (from scratch;
  the same constant-time ARX core the kernel/librheo DRBGs carry, exposing a
  caller nonce + block counter for the AEAD framing).
- **`poly1305`** - the 130-bit one-time MAC (from scratch; the public-domain
  "donna" 5×26-bit-limb form, `u64` partial products, `2^130 ≡ 5` reduction,
  constant-time tag compare).
- **`chachapoly`** - the ChaCha20-Poly1305 AEAD (from scratch, RFC 8439 §2.8:
  block-0 Poly1305 key, payload from counter 1, the `aad ‖ pad ‖ ct ‖ pad ‖
  len64 ‖ len64` MAC input, encrypt-then-MAC with a constant-time verify on open).
- **`hash`** (`sha2`), **`kdf`** (`hkdf` HMAC-SHA256 + HKDF-Expand-Label),
  **`kx`** (`x25519-dalek`), **`sign`** (`ed25519-dalek`), **`aesgcm`**
  (`aes-gcm`) - the audited crates wrapped behind rheo-net's own API.
- **`aead`** - the `Aead` seam (both AEADs implement it) + the **nonce-safe**
  `SealingKey`/`OpeningKey`.
- **`rand`** - the two-randomness-class boundary + the fork-epoch guard.

### Crate-backed vs from-scratch (the honest split)

From-scratch (ours): ChaCha20, Poly1305, ChaCha20-Poly1305. Crate-backed:
SHA-256/384 (`sha2` 0.10.8), HKDF (`hkdf` 0.12.4), X25519 (`x25519-dalek` 2.0.1),
Ed25519 (`ed25519-dalek` 2.1.1), AES-128/256-GCM (`aes-gcm` 0.10.3). **All five
crate-backed primitives build `no_std` on all three bare targets** - none needed a
from-scratch fallback. The one build hazard (the §3 build note): the intrinsics
backends miscompile under LLVM on `x86_64-unknown-none`, so the build forces the
**software** backends via `RUSTFLAGS` cfgs on all three ISAs (the scalar path the
posture wants). The `netcrypto-demo` bin is built by a dedicated xtask step
(`build_crypto_demo`) with those cfgs, exactly as N2c's smoltcp cell is.

### Two randomness classes, structurally separated

Conflating public randomness with key material is a silent nonce-reuse / key-leak
break, so the API keeps them in **distinct types with no bridge**:

- **Public** (`rand::PublicRandom`) - a fast ChaCha20 side-stream (seeded once
  from the attested per-cell DRBG) yielding **non-secret** integers/bytes only
  (DNS txids, cookies, hash seeds, jitter). It exposes no method that returns a
  key type.
- **Keys** (`kdf`) - derive via **HKDF over a transcript**, keyed from
  `kdf::Ikm`, whose only constructors are `from_attested()` (the attested per-cell
  DRBG) or `import()` (an explicit pre-shared / DH secret / test vector). There is
  **no path from `PublicRandom` to `Ikm`** - a public value cannot become keying
  material, enforced at compile time by the type system.

### The nonce-reuse hazard guard

A single `(key, nonce)` must never encrypt two messages. `aead::SealingKey` owns a
**monotonic 64-bit counter** and chooses the 96-bit nonce itself
(`iv_prefix ‖ counter`); the caller never supplies a nonce, so it cannot be
replayed, and the counter refuses to wrap. **Fork / checkpoint-restore** is the
other hazard (a restored image replays its counter): a `SealingKey` snapshots the
process **fork epoch** at creation, the cell calls `rand::bump_fork_epoch()` after
a fork/restore, and any surviving key then **refuses to seal**
(`NonceError::ReseedRequired`) until reseeded. Full checkpoint integration is later;
the API already forbids a replayed nonce. The low-level `Aead::seal`/`open` take an
explicit nonce - used only to replay the published RFC/NIST vectors.

### The proof (`netcrypto` test kernel, all 3 ISAs)

A `netcrypto-demo` cell (loaded like `netcore`, but **pure compute - no netdev**)
runs every primitive against its published vector, exiting `0x42` only if all pass:

1. **ChaCha20 block** = RFC 8439 §2.3.2 keystream.
2. **Poly1305** = RFC 8439 §2.5.2 tag.
3. **ChaCha20-Poly1305 AEAD** = RFC 8439 §2.8.2 ciphertext + tag, **plus** a
   decrypt round trip and a tampered-tag **and** tampered-ciphertext rejection.
4. **SHA-256 + SHA-384** of `"abc"` = the NIST / RFC 6234 digests.
5. **HKDF-SHA256** = RFC 5869 Test Case 1 PRK + OKM.
6. **X25519** = RFC 7748 §5.2 scalar mult **and** §6.1 a full Diffie-Hellman
   (both public keys derived, both sides' shared secret matched).
7. **Ed25519** = RFC 8032 §7.1 TEST 1 (empty message: derive pubkey, sign, verify)
   **and** TEST 3 (2-byte message), **plus** a tampered-signature and
   tampered-message rejection.
8. **AES-128-GCM** (GCM-spec / NIST TC4) **and** **AES-256-GCM** (TC16) encrypt +
   tag, decrypt round trip, and a tampered-tag rejection.
9. **Two randomness classes**: a key schedule from a fixed IKM derives
   deterministically (and a different IKM differs); `public_random()` is a
   distinct non-deterministic stream never equal to a key.
10. **Nonce-safe `SealingKey`**: two seals get **distinct** nonces, each opens
    correctly, and a `bump_fork_epoch()` makes the key refuse to seal.

Every expected value is a real published vector, **independently verified** before
being hardcoded (never computed by the crate under test). No kernel object / verb /
kernel change; the crypto crates are doc-named userspace deps (pinned, `no_std`,
feature-gated); no `cfg(target_arch)`.

### Deferred to N3b (explicit)

The **TLS 1.3 handshake** state machine + record layer (built on these
primitives), **X.509** certificate parsing + path validation, and the
**WireGuard/IPsec** protocol machinery. A **rustls-class** TLS and a
**boringtun-core-class** WireGuard remain the doc-named options for the protocol
layer (§3). Constant-time review of the from-scratch Poly1305/ChaCha20 against a
hardware side-channel model, and the arch crypto-instruction dispatch
(AES-NI/VAES/ARM-CE/RISC-V-vector, with the scalar path as the always-present
fallback and benchmark), ride with the perf substrate (N6/N8).

## 15. Phase N3b (done): TLS 1.3 - handshake + record layer + minimal X.509

N3b builds a working **TLS 1.3** (RFC 8446) client - and enough server side to
prove an in-cell handshake - on the N3a crypto primitives (§14). It adds **no
kernel object, no verb, no kernel change** (TLS is pure userspace), **no
`cfg(target_arch)`**, and **no new dependency** (it is from-scratch over N3a, so
the pinned crate inventory in §3 is unchanged). Everything is behind the `net`
crate's `tls` feature (which implies `crypto`), off by default and built
separately by xtask with the same force-soft backend cfgs as the crypto demo, so
the base stack + every existing test are unaffected.

### The architecture choice: from-scratch, not rustls (documented)

The plan named two paths - lean on `rustls` `no_std`, or build a minimal TLS 1.3
from scratch. A bounded rustls probe was run first: **rustls `=0.23.42` does build
`no_std`** for `riscv64gc-unknown-none-elf` with
`default-features = false, features = ["tls12","hashbrown","custom-provider"]`
(its transitive tree - `rustls-pki-types`/`rustls-webpki`/`hashbrown`/`zeroize`/
`subtle`/`untrusted` - is all `no_std`, and with `custom-provider` **`ring` is
not pulled** into the target build). So rustls itself is not the blocker.

It was set aside for three concrete reasons, all fatal to *this phase's proof*:

1. **The RFC 8448 KAT needs the intermediate key-schedule secrets**, and rustls'
   public API does not expose the early/handshake/master or per-stage traffic
   secrets - the very values the known-answer test checks. A from-scratch key
   schedule *is* what the KAT proves.
2. **rustls generates its own ephemerals**, so it cannot be driven with RFC 8448's
   fixed X25519 private keys to reproduce the trace.
3. **A full custom `CryptoProvider`** (cipher suites, key exchange, AEAD, HKDF,
   signature verify) would be needed to wire N3a's primitives into rustls - a
   large trait-glue surface for no proof benefit.

Given N3a already proved every primitive builds `no_std` and vector-matches, a
focused from-scratch TLS 1.3 over them gives full control and directly exercises
the key schedule the KAT pins. rustls (or a rustls-class stack) remains a
doc-named option for a *later* full client/server with session resumption / the
whole extension surface - N3c/N4.

### What the `net` crate adds (`net::tls`, feature `tls`)

- **`keyschedule`** - the RFC 8446 §7.1 key schedule over N3a's HKDF: `early_secret`
  -> `handshake_secret` (salt = `Derive-Secret(Early,"derived","")`, IKM = the
  X25519 ECDHE) -> `master_secret`; `derive_secret` = HKDF-Expand-Label over the
  transcript hash; `traffic_key`/`traffic_iv` (the `"key"`/`"iv"` labels);
  `finished_key` + `verify_data` (`"finished"` + an **HMAC-SHA256 built from
  scratch** over the audited `sha2` - the only MAC beyond HKDF and Poly1305).
  SHA-256 suites only (`HASH_LEN = 32`); a SHA-384 suite is deferred.
- **`record`** - the RFC 8446 §5 record layer: `RecordKeys` owns the AEAD + write
  IV + a per-direction **sequence counter**, and constructs the **per-record nonce
  = write_iv XOR (0-left-padded 64-bit seq)** (the classic footgun, done in one
  place). It frames `TLSCiphertext { opaque_type=23, 0x0303, length,
  AEAD(inner_plaintext) }` with the 5-byte header as AEAD additional data, and the
  `content || real_content_type || padding` inner-plaintext discipline. Both AEADs
  ride through the N3a `Aead` seam (a `Box<dyn Aead>`).
- **`msg`** - ClientHello/ServerHello build + parse (`key_share` X25519,
  `supported_versions` TLS 1.3, `supported_groups`, `signature_algorithms`), the
  generic handshake-message header framing, and the running `Transcript` (the
  SHA-256 over the concatenated handshake messages - kept as raw bytes so any
  prefix hash is one `sha256`, clearer than snapshotting an incremental hasher).
- **`x509`** - a from-scratch minimal DER walk: it extracts the signed
  `tbsCertificate` bytes, the **Ed25519 SubjectPublicKeyInfo** (OID 1.3.101.112),
  and the outer `signatureValue`, and verifies the certificate's Ed25519 signature
  (constant-time via `ed25519-dalek`'s strict verify). **Full chain / path / name
  validation is DEFERRED** (see below).
- **`handshake`** - `run_handshake(suite, ServerIdentity)`: a full 1-RTT handshake
  driven in-cell (client + server endpoints) - ClientHello/ServerHello, matching
  handshake + application traffic keys via the key schedule, then the authenticated
  flight (EncryptedExtensions / Certificate / CertificateVerify (Ed25519) / server
  Finished) over the record AEAD, the client verifying the cert signature, the
  CertificateVerify signature, and the server Finished, then its own Finished for
  the server to verify.

### Cipher suites / groups / signatures

- **Cipher suites**: `TLS_AES_128_GCM_SHA256` (0x1301) and
  `TLS_CHACHA20_POLY1305_SHA256` (0x1303) - the two mandatory-to-implement
  SHA-256 suites, both riding the N3a AEADs.
- **Groups**: `x25519` (0x001d).
- **Signatures**: `ed25519` (0x0807) for CertificateVerify and the certificate.
  ECDSA (`secp256r1`) and RSA are deferred (no RSA/ECDSA-verify primitive in N3a).

### The per-record nonce (correctness-critical)

RFC 8446 §5.3: the 64-bit record sequence number is encoded big-endian, **left-
padded with zeros to the 12-byte IV length**, and **XORed into the static write
IV**; the sequence resets to 0 when the keys change (handshake -> application).
`RecordKeys::nonce` does exactly this (the IV's high 4 bytes are never touched),
and the sequence advances per record - and, on decrypt, **only on a successful
open**, so a rejected record does not desynchronise the counter. Getting this
wrong is a classic silent break; it is pinned indirectly by the RFC 8448 write
keys/IVs matching and directly by the in-cell round trip + tamper test.

### keys-as-capabilities (API-level)

The negotiated traffic keys live inside `record::RecordKeys` (and the N3a
`aead::SealingKey`) with **no getter that returns the raw key bytes** - the
application seals/opens through the object but cannot extract the key material.
This is the *API shape* of keys-as-capabilities. The full mechanism - a key
**programmed into a NIC TX/RX queue as a capability, never readable back**, with
encrypted zero-copy fetch - is **N8** (§5); N3b establishes the non-extractable
key handle as the seam N8 hardens.

### Minimal X.509 scope + why it is enough here (documented)

`x509::parse` does a **minimal** DER walk - `tbsCertificate` + Ed25519 SPKI +
`signatureValue` + a signature verify. **Deferred (explicit)**: issuer-chain / path
building, validity-date checks, name / SAN matching, basicConstraints / EKU /
key-usage enforcement, and ECDSA/RSA SPKIs. This is **sufficient for the downstream
Tor/onion consumer**, which validates peer identity **out of band** (the onion
descriptor's own signature over the service identity key), so a full PKI path is
not the trust anchor on that route - only that the CertificateVerify was signed by
the presented key, which N3b proves.

### The proof (`nettls` test kernel, all 3 ISAs)

A `nettls-demo` cell (loaded like `netcrypto` - pure compute, **no netdev**) proves
TLS 1.3 three ways, exiting `0x42` only if all pass:

1. **RFC 8448 §3 known-answer test (the authoritative TLS 1.3 oracle).** Fed the
   RFC's own ClientHello/ServerHello bytes + X25519 private/public keys, the key
   schedule derives the RFC's values **byte-for-byte**: the ECDHE secret (both
   directions), the transcript hashes (`SHA-256(CH‖SH)` and `SHA-256(CH‖SH‖server
   flight)`, computed from the message bytes, not taken from the RFC), the Early /
   Handshake / Master secrets, the client & server **handshake** and **application**
   traffic secrets, the exporter-master secret, the handshake + application **write
   keys and IVs**, and both Finished **verify_data** MACs. Every expected value was
   fetched from the authoritative RFC 8448 text (not recalled) and is hardcoded as
   the answer, never computed by the code under test.
2. **In-cell full 1-RTT handshake**, for **both** cipher suites: a client and
   server endpoint complete a handshake, derive **matching** traffic keys
   (asserted equal on both sides), exchange an encrypted application record **both
   directions** (plaintext round-trips exactly), and a **tampered record fails**
   the AEAD (returns an error, does not decrypt). The server authenticates with a
   real Ed25519 certificate; the client verifies the cert signature, the
   CertificateVerify signature, and the server Finished, then sends its own
   Finished which the server verifies.
3. **Minimal X.509**: a known Ed25519 self-signed test certificate (generated once
   by `openssl req -x509 -newkey ed25519`, hardcoded as DER - a real cert, not
   committed as a fixture) is parsed, its subject public key extracted and its
   Ed25519 signature **verified (pass)**, and a **tampered tbsCertificate is
   rejected (fail)**.

No kernel object / verb / kernel change; TLS is a doc-named-crate-free from-scratch
userspace layer (the N3a crates it reuses were already pinned); no
`cfg(target_arch)`. On x86-64 the `tls` build uses the same force-soft AES/GHASH/
curve25519 backend cfgs as the crypto demo (the intrinsics backends miscompile
under LLVM on `x86_64-unknown-none`, §3).

### Deferred past N3b (explicit)

- **TLS 1.2** (a separate handshake + record construction) - not built.
- **Full X.509 chain / path / name validation** (above) and ECDSA/RSA cert
  signatures - deferred; minimal Ed25519 signature verify is the deliverable.
- **Live HTTPS over the network** - deferred (N3c/N4): SLIRP has no deterministic
  TLS server to handshake against (the same reason N2a/N2b deferred a live TCP
  peer), so a live handshake would be non-deterministic; faking one would be
  dishonest. The in-cell handshake + the RFC 8448 KAT are the real proof. A live
  HTTPS GET rides in with the N4 service-cell model / an external peer at the
  hardware lab.
- **Key update** (`KeyUpdate` post-handshake rekey), **0-RTT / early data**,
  **session resumption / PSK / tickets**, **HelloRetryRequest**, client
  authentication, and the wider extension surface (ALPN, SNI parsing, etc.) - all
  deferred; N3b is the 1-RTT handshake + record layer + minimal X.509 slice.
- **WireGuard / IPsec** remain the N7 security-transport work (the N3a primitives
  and this key-schedule/record machinery are the shared foundation).

## 16. Phase N2d (done): true async receive - the NIC RX interrupt + a park

Everything from N1 to N3 could *send* asynchronously but not *wait*. `librheo::net::
send`/`mac` were genuine async submissions (park the strand on the completion token,
wake on the CQ entry), while **`recv` was a busy re-poll**: `OP_NET_RX` returned
"nothing available" and the cell submitted it again. The reactor had slots for the
console, the timer, a child wait and the cross-cell channel - but **no network
slot** - so a strand waiting for a packet was always "ready", `block_on` never
reached its idle path, and a cell waiting for a frame **spun a whole core**. The
OS's 0%-CPU-idle story (docs/LIBRHEO.md Phase D/F) was true for the console and the
timer but not for the network. N2d closes that.

### The three pieces

**1. The kernel wait (`kernel/src/net_rx.rs`, portable).** `SYS_WAIT_NET (48)`:
`wait_net(buf_va, len, timeout_ns) -> frame_len`. Blocks until a received frame is
available, copies it into the cell's buffer (the cell's address space is active
during the trap, so **one copy**, straight from the virtqueue buffer the device
DMA'd into), and returns its length. `timeout_ns = 0` waits indefinitely; a
non-zero deadline is the "**a frame, or the RTO, whichever comes first**" primitive
every transport needs - where both the NIC and the timer interrupt are wired the
kernel arms the deadline and halts **once**, waking on either source.

Mechanism only, **no new kernel object** (ARCHITECTURE.md 6): it exposes the same
virtio-net driver the `OP_NET_*` opcodes already bridge to, in the shape of the
existing `SYS_WAIT_INPUT` block-and-wake (docs/LIBRHEO.md Phase D) - the direct
precedent. The alternative considered was a *blocking flag* on the `OP_NET_RX`
submission; it was rejected because the queue drain runs inside the cell's
`SYS_DOORBELL` trap and processes the **whole** submission ring, so a blocking
opcode would stall every other strand's completions behind one packet. Keeping the
block in a wait verb leaves the decision *when to block* with the reactor, which
only blocks after every strand has parked - exactly the console/timer model.

**Where the received frames are buffered.** `input.rs` needs its own byte ring
because the 16550's 16-byte FIFO is the only other buffer. A NIC does not: the
driver pre-posts **16 RX buffers of 2 KiB** on the receive virtqueue, the device
DMAs each arriving frame into one of them, and the used ring records the arrival.
That *is* the kernel-side RX ring - frame-pool memory written by the device, so a
frame arriving while a cell computes is not lost. A second kernel ring would only
add a copy, so `net_rx.rs` deliberately does not add one: the interrupt handler
records the arrival, the wait path copies once into the cell.

**2. The NIC RX interrupt (per-ISA, `kernel/src/arch/*`).** The driver
(`hw/virtio_net.rs`) records the **virtio-mmio transport slot** it bound to - a
portable fact - and `net_rx::enable_irq()` hands it to `arch::enable_virtio_net_irq
(slot)`, which turns it into that ISA's interrupt id. The RX virtqueue leaves
`avail.flags` clear (that is what asks the device to raise its line on a received
frame); the TX ring now sets `VRING_AVAIL_F_NO_INTERRUPT`, since transmit
completions are polled and their interrupts would only be spurious wakeups. The
handler (`net_rx::on_irq`) acknowledges the device through virtio-mmio's
`InterruptStatus`/`InterruptACK` (so its level-triggered line drops) and counts the
arrival. Interrupt bring-up is **opt-in** - only the `netwait` test kernel calls
`net_rx::enable_irq()`, so every other kernel boots byte-for-byte as before.

RISC-V needed one more thing: S-mode interrupts are **always enabled while
executing in U-mode** (`sstatus.SIE` gates them only in S-mode), so once a device
IRQ is wired a frame arriving mid-cell traps in the U-mode path. `riscv_user_trap`
now recognises an interrupt cause, services the device, and resumes the cell at the
interrupted instruction (before, it would have been read as a fault). ARM64 cells
run at EL0 with IRQ masked, so the interrupt simply stays pending until the kernel's
next idle - the wait takes it there.

**3. The reactor network slot (`librheo/src/rt.rs`).** `net_rx_req` joins
`console_req`/`timer_req`/`wait_req`/`chan_recv_req`, serviced in `block_on`'s idle
path: `rt::recv_frame(buf, len, timeout_ns)` parks the strand on a token, and when
every strand has parked the reactor blocks in `SYS_WAIT_NET` and wakes the parked
one with the frame. `rt::net_wakeups()` counts the deliveries - **one park and one
wake per frame**, which is the no-spin evidence the test asserts.

`librheo::net` becomes symmetric: **`recv`** parks until a frame arrives,
**`recv_timeout`** parks with a deadline, and **`try_recv`** is the non-blocking
drain a batching transport uses (`net::wire::recv_frame`, `net::arp::resolve`'s poll
loop, and the smoltcp cell's RX batch all use `try_recv`, so their behaviour is
unchanged - the last frame of a burst must not block).

### Per-ISA interrupt status (honest)

| ISA | NIC transport | RX interrupt path | Receive wait |
|---|---|---|---|
| **RISC-V 64** | virtio-mmio (slot 7 on QEMU `virt`) | APLIC-S source `1+slot` in MSI mode -> this hart's IMSIC S-file (identity `16+slot`) -> `sip.SEIP`; dispatched by identity in `handle_ext_irq` | **interrupt-driven**, halts at `wfi` - genuine 0%-CPU park |
| **ARM64** | virtio-mmio (slot 31 on QEMU `virt`) | GICv3 SPI `16+slot` (INTID `48+slot`) -> GICD -> `ICC_IAR1_EL1`, EOI via `ICC_EOIR1_EL1` | **interrupt-driven**, halts at `wfi` - genuine 0%-CPU park |
| **x86-64** | virtio-**pci** (q35, driven through the `VIRTIO_PCI_CAP_PCI_CFG` tunnel) | **none wired** - see below | **kernel poll** (bounded): the cell still parks once, but the CPU spins |

x86-64 is a documented fallback, not a claim. The NIC there is driven *entirely
through PCI configuration space* because PVH boot has no firmware to program BARs -
so there is no mapped BAR to hold an MSI-X table - and legacy INTx would ride the
same IOAPIC path that, under QEMU TCG + `kernel-irqchip=split`, does not re-deliver
reliably (the same reason the x86-64 UART RX line stays a poll, docs/LIBRHEO.md
Phase D; its *timer* LAPIC path does work). The honest x86-64 result is therefore:
the userspace re-submit storm is gone (one park per receive, the real improvement),
but the kernel-side wait is a bounded spin, and `net_rx::interrupt_driven()` /
`did_idle()` both report false there. **Programming the MSI-X table through the
config tunnel** (the table lives in a BAR the tunnel can reach) is the specific
next step.

### The proof (`netwait` test kernel, all 3 ISAs)

`librheo-netwait` runs as a cell over a real virtio-net device on QEMU's SLIRP user
netdev (deterministic, network-free - the same setup as `librheonet`):

1. read the MAC, then **drain** the receive queue with `try_recv` so the waits start
   empty; spawn a **witness** strand that counts its own resumptions;
2. send a broadcast **ARP request** for the gateway `10.0.2.2` and `net::recv().await`
   - the strand **parks**; SLIRP's ARP reply is the wake, asserted to be a reply
   whose sender IP is the gateway;
3. send a **TCP SYN** to a closed port on the gateway and park again - SLIRP's reset
   is a second real frame through the same blocking path;
4. park once more with a **20 ms deadline** on an empty queue with nothing in
   flight: no frame can arrive, so the kernel arms the deadline and **halts the
   CPU** until it fires, returning 0.

Asserted: the cell exits `0x42` only if both frames are the expected replies, the
witness advanced **while the receiver was parked** (so the receive genuinely
suspended instead of holding the vcore), the bounded wait returned empty at its
deadline, and `rt::net_wakeups()` equals the number of receives - **exactly one
park + one wake each, never N re-polls**. The kernel then asserts, on the
interrupt-driven ISAs, `net_rx::irq_count() > 0` - the count is only incremented
from the ISA's interrupt vector, so it cannot be faked; two genuine NIC interrupts
are taken per run - and `net_rx::did_idle()`, that the wait really halted the CPU.
On x86-64 the kernel prints the poll-fallback line instead of asserting either.

One nuance, stated plainly: under SLIRP both replies are queued **during** the
guest's transmit (SLIRP answers ARP and refuses the SYN inside the TX handler), so
the frame is already there when the wait begins - the interrupt is genuinely
delivered and taken, but the wait does not have to halt for it. The halt is proven
by the bounded phase, which cannot be satisfied by any frame. Both properties are
real; neither is dressed up as the other.

### What N2d defers (explicit)

- **x86-64 MSI-X** (above) - the remaining ISA gap.
- **Interrupt coalescing / NAPI-style batching**: every received frame raises an
  interrupt today. The sDDF armed-doorbell + deferred-notify protocol (§1) is where
  batching belongs, and rides with the N6 offload/multiqueue work.
- **Waking a *different* cell on a frame**: the wait blocks the calling cell's
  vcore (its strands are all parked by then). A frame steering to another cell needs
  the multi-cell scheduler / steering grants (SMP, task #27; §4).
- **Racing a receive against other reactor sources** (a channel message, console
  input) in one halt: the reactor services one idle source per pass, and the
  deadline is the only second wake source the kernel wait itself arms.
- **Zero-copy receive**: the wait still copies from the virtqueue buffer into the
  cell's buffer. Landing payloads directly in a cell's arena pages (header/payload
  split + grant DMA) is N6.

## 17. Phase N4a (done): the network service cell + concurrent fan-out

This is the **keystone** of the whole roadmap. Doctrine puts the network stack in
userspace (docs/ARCHITECTURE.md 4.7, §§5-9 of docs/NETWORKING.md): a long-lived
**service cell** owns it and other cells reach the network by talking to that cell.
Phase E proved *one* cell can talk to *one* cell. The load-bearing question is
whether **one cell can serve many, concurrently** - because every remaining item
needs exactly that: app-protocol servers (N5), the remote-INET bridge for Linux
binaries (N4b), onion routing, DHCP/zeroconf/NTP (N4c). N4a closes it.

### The gap that had to be closed

Before this phase a cell held **exactly one** channel end. `SYS_CONNECT` reported
that one end, and Phase J's `SYS_SPAWN` inherited that one end into a child. Three
spawned children would therefore all inherit the **same ring region** - three
producers on one SPSC ring, which is not a fan-out but a race. And `SYS_SWITCH` is
a *directed* `cur^1` hand-off: from client cell 2 it reaches cell 3, never the
service at cell 0, so a 1-service + 3-client topology livelocks between siblings.

### The architecture choice: composition, with a minimal mechanism extension

Pure composition over the existing verbs is impossible for the reason above, so N4a
takes the **minimal mechanism extension** of the existing spawn/channel path -
option (a) of the two the plan named. It adds **no kernel object** (every piece
composes Cell, object 1, with QueuePair, object 3, exactly as the L6 pipe and Phase
J channel-inheritance precedents do) and no ambient authority. Three changes:

1. **A per-cell channel *table*** (`MAX_CELL_CHANNELS = 4`, a fixed static array -
   the kernel still allocates nothing). Slot 0 is the Phase E/J channel; slots 1..
   let a service hold one end per client. Each slot is a **separate ring region**,
   at `channel_slot_va(slot) = USER_CHANNEL_VA + slot * REGION_SIZE` (24 GiB +).
   `SYS_CONNECT(out, slot)` gained the slot argument and reports `count` (how many
   ends the cell holds) alongside `chan_va`/`cap_id`/`role`. Every pre-existing
   caller passes slot 0 and is unchanged.
2. **`SYS_SPAWN` gained a `chan_spec` argument**: 0 = the Phase J default (inherit
   slot 0 if wired), else `SPAWN_CHAN_SLOT | slot << 8` = inherit the caller's slot
   `slot`. The child always receives the inherited end at **its own slot 0** with
   the opposite role, so a client binary is slot-agnostic and identical whichever
   client it is. `AddressSpace::share_rw_into` gained the matching `dst_base` (its
   read-only sibling `share_ro_into` already had one).
3. **`SYS_YIELD` (49)**: hand the CPU to the **next runnable native cell** in
   round-robin order; the caller stays runnable. This is the N-cell generalisation
   of `SYS_SWITCH`, and it is *the same cooperative cross-cell scheduler*
   `SYS_WAIT`/child-exit already drive in `kernel/src/nproc.rs` - exposed as a plain
   yield, transferring no authority (the cells share one capability bundle). Where
   the caller has no native process tree (two cells a test kernel wired but never
   spawned) the round-robin degenerates to `cur^1`, so the Phase E/J two-cell
   behaviour is byte-for-byte unchanged. The reactor's channel idle path now calls
   it instead of `sys::switch()`.

**A name-based rendezvous is deliberately out of scope.** Letting an *unrelated*
cell connect to a pre-existing service by name is a genuinely new capability (a
kernel-held namespace, and an authority question `spawn` answers structurally
today), so it would need an ARCHITECTURE.md §6 justification of its own. It is the
documented follow-on. N4a's fan-out is **parent-shaped**: the service spawns its
clients, which is enough for every N4b-N5 scenario.

### What the `net` crate adds (`net::service`)

- **`Service`** - `bind()` opens every channel end this cell holds and splits each
  into the Phase J async `AsyncSender`/`AsyncReceiver`; `spawn_client(slot, path,
  argv)` spawns a client cell handing it slot `slot`; `serve()` spawns **one strand
  per client**, each parked on its own receiver, and joins them. A strand answers
  its client's requests and replies on that client's channel - so a slow client
  blocks nobody.
- **`Client`** - the thin end a spawned cell uses: `open(id)` binds the channel it
  inherited (its slot 0), then `echo`/`resolve`/`bye` send a request and await its
  response.
- **The protocol is one word each way**: a `Request { op, client, seq }` packed into
  the channel message's `tag`, with the argument/result in its 32-bit `val` - the
  async channel's symmetric payload. `OP_ECHO` returns a **per-client keyed**
  transform (`echo_transform`, so a client can tell its own answer from a sibling's);
  `OP_RESOLVE` maps a catalogue name id to an IPv4 address (`u32` is exactly an A
  record) answered from the **network-free tiers of `net::dns`** - a `HostsTable`
  and a TTL `Cache`; `OP_BYE` returns the request count and ends that strand.
- **`ServiceReport`** - the ledger `serve()` returns: per-client served counts, the
  processing order, the in-flight high-water mark, per-client reactor wakeups, and
  the live-op result.

Two reactor additions back it (`librheo/src/rt.rs`): `attach_channel_slot` +
`chan_send_on`/`chan_recv_on` per slot (the reactor scans slots in order, which is
what round-robins the per-client strands), and two witnesses -
`chan_wakeups_on(slot)` and `chan_max_pending()`, the latter measured with a new
**non-destructive** `Qp::sq_pending`/`cq_pending` peek.

### Word-wide protocol: the honest simplification

A name is an id, not a string, and an answer is one `u32`. That is a real
simplification, chosen so what the phase proves is the *fan-out* rather than a
marshalling layer. The general form already exists in the substrate: a request too
big for a word goes in a **sealed grant shared over the same channel** (Phase E
`ipc::share` - zero-copy, capability-checked), and that is the documented follow-on
for N4b/N5, where an HTTP header block or a DNS name has to travel.

### Concurrency, not parallelism (honest)

One CPU, cooperative scheduling. The service's strands genuinely interleave (all N
requests in flight, N strands making progress, no client blocking another) and the
cells hand the CPU on at syscall boundaries. **Nothing runs simultaneously**: a
service strand cannot compute while a client computes. That needs SMP (task #27),
and until then a "concurrent service" is exactly that - concurrent. Everything §17
claims is a concurrency claim, and the test's witnesses measure concurrency.

Also honest: the async in-cell wait is a genuine reactor park (asserted per client
via `chan_wakeups_on`), but the cell-boundary hand-off is still the cooperative
yield - the same standing Phase J caveat.

### The proof (`netservice` test kernel, all 3 ISAs)

The kernel wires a service cell (cell 0) with **three** channel ends - three
separate ring regions at slots 0-2 - a queue pair, and a **cell-spawn** capability,
seeds a ramfs with `/bin/netsvc-client`, and runs it. The service then:

1. binds all three ends (`Service::bind`), seeding `alpha`/`beta` into a
   `dns::HostsTable` and `gamma` into a `dns::Cache` - so the three clients' answers
   come from **both** network-free resolution tiers;
2. reads the NIC MAC, enabling the bonus live op;
3. spawns `/bin/netsvc-client` **three times**, client k on the service's slot k;
4. runs `serve()` - one strand per client - then reaps all three children.

Each client sends a **distinct** request set and predicts every answer exactly: a
per-client echo (`echo_transform(0xA5A50000 | id, id)`), its own catalogue name
(`10.1.1.1` / `10.2.2.2` / `10.3.3.3`), and a `BYE` whose reply is its own request
count. Client 0 additionally asks for the live gateway resolve.

The service exits `0x42` only if **all** of this holds, identically on all three
ISAs:

| Assertion | Observed |
|---|---|
| distinct correct response per client | each client verifies its own echo + name and exits `id+1`; the service asserts the codes are exactly `1,2,3` |
| per-client work counted | `served == [4, 3, 3]` (client 0 does the extra live op) |
| **interleave witness** | `order == [0,1,2, 0,1,2, 0,1,2, 0]` - the exact round-robin: strand k reaches round r only after strands `0..k` did, so no strand monopolised the vcore |
| **in-flight witness** | `max_in_flight == 3` - all three clients' requests were queued **at the same instant**, before the first reply went out |
| no spin | `wakeups == [4, 3, 3]` - one genuine reactor park+wake per message, per client |
| children reaped | all three waited, each with its distinct exit code |

The **deterministic core is network-free**: pre-seeded hosts/cache tiers, no packet
required. The **bonus live op** is one real ARP for the SLIRP gateway `10.0.2.2`
that the service performs *inside client 0's serving strand* - it parks on the wire
(rheo-net N2d) while its sibling strands keep running - and it **degrades honestly**:
with no NIC, or no reply, it reports `REPLY_NONE`, the client accepts that, and the
run still passes. On all three ISAs under SLIRP it does resolve, and the log says so.

### What N4a defers (explicit)

- **Name-based rendezvous** (above): an unrelated cell connecting to a running
  service by name. The genuinely new capability; needs its own §6 justification.
- **Requests larger than a word**: pass them in a sealed grant over the same
  channel (the Phase E mechanism already exists). Needed by N4b/N5.
- **More than `MAX_CELL_CHANNELS` (4) clients**: a real service wants hundreds, which
  means a per-cell channel *region* whose slots are allocated dynamically, not a
  fixed array. The fixed array keeps the kernel allocation-free today.
- **Parallel service** (SMP, task #27) - see above.
- **Service restart / client disconnect**: a client that dies mid-request leaves its
  strand parked; supervision (and channel teardown on reap) is future work.
- **Steering**: a received frame still wakes the *calling* cell. Waking the service
  cell on a frame destined for a client needs the N6 steering grants.

## 18. Phase N4b (done): remote INET for unmodified Linux binaries

The single biggest functional unlock left. **L8-INET** (§10) gave Linux cells
`AF_INET`/`AF_INET6` sockets, but **loopback only**: `kernel/src/linux/inetsock.rs`
refused every non-loopback destination with `-ENETUNREACH`, because a TCP connection
between two *local* endpoints degenerates to the L6 ring pair the kernel already has,
while a *remote* one needs the real segment/RTO/congestion machinery. N4b makes real
remote destinations work, so an **unmodified static-glibc binary does a real DNS
query, a real UDP round trip, and a real TCP connect over the NIC**.

### The blocker, and why it was only half true

L8-INET documented the obstacle as: *"the kernel is allocation-free, so the
alloc-based `net::tcp`/`net::udp` cannot be linked kernel-resident."* That is true of
the **`kernel/` library** - and it must stay true. It is **not** true of a *kernel
binary*: `tests/src/*.rs` kernels declare their own `#[global_allocator]`
(`runtime::Heap`) and already link alloc-using crates (`posix`). So the stack can be
linked *beside* the kernel by whoever owns the machine's network - which is exactly
the shape doctrine wants.

One real obstacle remained: `rheo-net` depended unconditionally on **librheo**, which
supplies a *cell's* `_start`, `#[panic_handler]` and `#[global_allocator]`. Linking
that into a kernel binary is a duplicate-lang-item error. N4b therefore gives the
crate two **postures** (one code base, no duplication):

- **hosted** (`hosted` feature, on by default - every existing cell build): the
  librheo-driven async endpoints and services - `arp::resolve`, `udp::UdpEndpoint`,
  `icmp`, `dns`, `timer`, `local`, `service`, `smoltcp_cell`, `crypto`/`tls`.
- **codec** (`--no-default-features`): the pure, synchronous layers - `eth`, `ip`,
  `arp` packet build/parse, `udp` build/parse + checksum, `tcp` (the whole RFC 793
  state machine), `cc`, `shard`, and `wire`'s framing/parsing. No librheo, so it
  links into a kernel binary. The only duplicated item is `eth::Mac`, a six-byte
  newtype declared locally when librheo is absent.

`tcp::Connection`'s seam is what makes this work: `poll(now) -> Option<Vec<u8>>` and
`on_wire_segment(now, bytes)` are **synchronous and transport-independent** (they were
written that way for the N2a in-cell virtual link), so the same state machine drives
from kernel context with no async runtime at all.

*Build caveat (recorded because it is a real trap):* never build `-p qemu-tests` and
`-p rheo-net` in **one** cargo invocation - feature unification would re-enable
`hosted` and the test kernel would fail to link. xtask and CI keep them separate.

### The architecture: `svc::SocketOps`, the `FileOps` precedent

The kernel gains **a bridge, not a network stack**, and **no kernel object**:

- `kernel/src/svc.rs` gains **`SocketOps`** - a table of `fn` pointers
  (`local_ip`, `udp_bind`/`udp_close`/`udp_send`/`udp_recv`/`udp_pending`,
  `tcp_connect`/`tcp_send`/`tcp_recv`/`tcp_close`) plus `set_socket_ops` /
  `socket_ops`, mirroring `FileOps`/`set_file_ops` line for line.
- `kernel/src/linux/fd.rs` forwards **non-loopback** socket operations to it. Two new
  `FdKind` variants hold the bridge handles (`InetUdpRemote`, `InetTcpRemote`); a UDP
  socket migrates onto the remote datapath the first time it names a non-loopback
  address. **Loopback is untouched** - `InetDgram`/`InetConn` and the L6 ring path are
  byte-for-byte as before, and `linuxinet` still asserts its exact transcript.
- With **no bridge registered** every non-loopback address still answers
  `-ENETUNREACH`, so all 49 pre-existing kernels behave identically.

This is the same doctrine that keeps the kernel **filesystem-free** while serving
`open`/`read`/`write`: mechanism in the kernel, policy in a registered service.
Alternative considered and rejected for now: forwarding to the **N4a service cell over
a channel**. It is the better end state (and the table is deliberately shaped to accept
that substitution unchanged), but it needs a *name-based rendezvous* so an arbitrary
Linux cell can reach a service it did not spawn - explicitly deferred by N4a §17. The
`FileOps` parallel is proven, synchronous-friendly and doctrine-clean, so it lands
first.

### Driving the NIC kernel-side

`net::udp`/`net::tcp`'s hosted endpoints reach the NIC through `librheo::net`
(`OP_NET_*`), which is the **cell** path. Kernel-side the bridge drives the driver
directly, through three small documented accessors added to
`kernel/src/hw/virtio_net.rs` - `send_frame_slice`, `recv_frame_slice`, `mac_addr`.
They are the same one-copy paths as the existing `tx`/`rx`/`mac` opcode bridges with
the queue-completion wrapper removed: **mechanism only, no new object**. A
`net_rx::wait_frame_slice` twin of `wait_frame` lets the bridge park on a
kernel-owned buffer.

A `Device`-trait refactor of the `net` transports (the N2c `smoltcp_cell`
`phy::Device` precedent) was the alternative. It was not taken: the hosted endpoints
are `async fn`s all the way down, so a trait seam alone would not make them callable
from a synchronous trap - the *codec posture plus a driver loop* is the smaller,
honest change. A device seam remains the right move when the datapath moves into a
service cell.

### Blocking: a park, not a spin

A remote receive blocks in **`net_rx::wait_frame_slice`** - the N2d
park-until-frame primitive with a deadline. On **riscv64/aarch64** the kernel
genuinely halts at WFI until the NIC's RX interrupt fires; on **x86-64** it falls
back to the documented bounded kernel poll (its NIC is virtio-pci through the
config tunnel: no mapped BAR for an MSI-X table, and legacy INTx rides the QEMU-TCG
IOAPIC path that does not re-deliver). Identical honesty to §16.

### What is on the wire

Everything is the stack's own code, driven by `tests/src/inet_personality.rs` (the
sibling of `vfs_personality.rs`):

| Layer | Code | Notes |
|---|---|---|
| L2 | `net::eth` | frame build/parse |
| ARP | `net::arp` | request build + reply parse; next hop = the destination on our own /24, else the gateway (a real routing decision), cached (4 entries) |
| L3 | `net::ip` + `net::wire` | IPv4 header + the ones-complement checksum, TTL 64 |
| UDP | `net::udp` | build/parse + the pseudo-header checksum, verified on receive |
| TCP | `net::tcp::Connection<FixedWindow>` | the full RFC 793 state machine: SYN, RTO retransmit, RST → CLOSED |
| identity | fixed | SLIRP's `10.0.2.15`, gateway `10.0.2.2` (DHCP as a userspace service is a later phase) |

### Scope: what works remotely, precisely

- **UDP: fully.** `sendto`/`recvfrom`, `connect`+`send`/`recv`, source-address
  reporting, ARP next-hop resolution, real IPv4+UDP checksums, and a receive that
  parks on the wire.
- **TCP: connect is real and proven.** The SYN goes out over the NIC, the RTO
  retransmits inside the budget, and SLIRP's **real reset** is turned by
  `tcp::Connection` into `ECONNREFUSED`; the deadline yields `ETIMEDOUT`. TCP **data
  transfer is implemented** (`tcp_send`/`tcp_recv` over the same `Connection`,
  reached by `read`/`write` on the fd) but is **not proven in QEMU**: SLIRP offers no
  TCP responder, so there is no deterministic network-free data round trip to assert.
  It is therefore honestly **untested code** until a phase adds a responder (a
  `guestfwd`ed listener, or the N4a service cell talking to a peer cell).

### The proof: the `linuxnet` test kernel

`tests/src/linuxnet.rs` + `tests/linux-fixtures/inetremote.c` - an **unmodified
static-glibc C binary** (built from source by xtask, gitignored, the `inet.c`/
`af_unix.c`/`hello.c` recipe) running as a `Personality::Linux` cell:

1. hand-build a DNS query (`A example.com`, txid `0x1234`) and `sendto` it to
   SLIRP's built-in responder **10.0.2.3:53**, then `recvfrom` the reply and assert
   its **structure**: the transaction id echoed, the QR bit set, and the sender being
   `10.0.2.3:53`. **Never** a specific resolved address - SLIRP proxies to the host
   resolver, so an A record's value is not deterministic.
2. `connect()` to **10.0.2.2:9** (a closed port on the gateway) and assert
   `ECONNREFUSED`.

Each phase prints one line from a small fixed set, so the transcript stays **exact**
while the program never fabricates a result: the accepted UDP lines are
`dns answers yes` / `dns answers none` / `dns no reply`, and the accepted TCP lines
`tcp refused` / `tcp timeout` / `tcp connected`. The kernel enumerates the accepted
products, reports which occurred, and requires that a structurally valid DNS reply
did arrive. It additionally asserts the receive genuinely parked
(`net_rx::irq_count() > 0` and `did_idle()` on the interrupt-driven ISAs). With no
netdev attached it skips-with-reason (loopback coverage lives in `linuxinet`).
Observed on all three ISAs: `dns answers yes` + `tcp refused`, exit 0.

### Invariants held

- **No new kernel object, no new syscall verb.** `SocketOps` is a bridge, the
  `FileOps` precedent; sockets remain per-cell synthesized fds.
- **The kernel stays network-stack-free and allocation-free.** The stack lives in the
  linked `net` crate on the registrant's side; `kernel/` gained three driver
  accessors and one wait twin, nothing else.
- **Loopback INET is unchanged** and `linuxinet` passes byte-for-byte.
- **No new external dependency**; **no `cfg(target_arch)` outside
  `kernel/src/arch/`**; `unsafe` stays in small documented blocks.

### What N4b defers (explicit)

- **IPv6 remote** - `AF_INET6` to a non-loopback address is still `-ENETUNREACH`.
- **A remote listener / `accept`** - an inbound connection needs the N6 NIC
  flow-steering grants (docs/NETWORKING.md).
- **A proven remote TCP data round trip** (above).
- **The datapath as a service cell** rather than a registered kernel-side table -
  blocked on N4a's name-based rendezvous.
- **Reference-counted remote handles** across `dup`/`fork`; **per-cell** datapath
  instances; larger registries (4 UDP endpoints, 4 TCP connections, 4 ARP entries);
  `SO_RCVTIMEO`/`O_NONBLOCK` (one documented 2 s receive / 3 s connect bound);
  **DHCP** (the SLIRP identity is fixed); non-blocking readiness for a remote TCP
  socket in `epoll`.
