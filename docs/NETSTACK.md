# rheo-net: the greenfield network stack

**Status:** Building. Phase **N1a** (the L2/L3 core), **N1b's L4** (UDP + ICMP),
**N1c's caching DNS client**, **N1e's TTL / hop-limit + traceroute**, **N1d's
local sockets** (native `net::local` + Linux AF_UNIX), **N2a's native TCP core**,
and the **L8-INET** personality slice (AF_INET/AF_INET6 sockets + a minimal epoll
over the **loopback** interface - §10(C), docs/LINUX-COMPAT.md) are done; the full
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
  **x25519-dalek**, **ed25519-dalek**, **poly1305**; a **rustls**-class TLS; a
  **boringtun**-core-class WireGuard. **smoltcp** is the blessed correctness-first
  transport for control/low-rate cells (Redox precedent); the HFT/warehouse hot
  lines use a native, sharded, zero-copy transport instead.
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
(build on the Phase D IRQ path), **zero-copy grant DMA to the wire** +
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
  - **N2b (next): congestion control + the two transports.** CUBIC/BBR as
    `CongestionControl` impls (the N2a seam), the smoltcp blessed correctness cell,
    and the native sharded/zero-copy transport; a live TCP echo / HTTP GET to SLIRP.
- **N3 - TLS 1.3 + HTTPS.** Crypto crates wired; keys-as-capabilities.
- **N4 - Service-cell model + fan-out + host services** (DHCP + zeroconf + NTP).
- **N5 - App protocols.** HTTP/2, gRPC, Arrow Flight (warehouse), Kafka.
- **N6 - Perf substrate.** NIC RX IRQ, zero-copy DMA + offload/multiqueue/RSS,
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
window - flow control - dominates). CUBIC/BBR are **N2b**, a drop-in
`impl CongestionControl`; `Connection` is generic over `C` so swapping the
controller is a type parameter, not a rewrite.

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
  N2b.
