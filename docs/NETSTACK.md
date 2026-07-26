# rheo-net: the greenfield network stack

**Status:** Building. Phase **N1a** (the L2/L3 core), **N1b's L4** (UDP + ICMP),
and **N1c's caching DNS client** are done; the full roadmap (N1-N8) is below.
This document is the architecture + roadmap + crypto posture;
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
    table (§8). Still open in N1: `local`/AF_UNIX (the zero-copy local path + the
    Linux AF_UNIX personality) - the next slice.
- **N2 - TCP + congestion control + the two transports** (native sharded + the
  smoltcp cell); proof: a TCP echo / HTTP GET to SLIRP.
- **N3 - TLS 1.3 + HTTPS.** Crypto crates wired; keys-as-capabilities.
- **N4 - Service-cell model + fan-out + host services** (DHCP + zeroconf + NTP).
- **N5 - App protocols.** HTTP/2, gRPC, Arrow Flight (warehouse), Kafka.
- **N6 - Perf substrate.** NIC RX IRQ, zero-copy DMA + offload/multiqueue/RSS,
  timer wheel; the socket/steering kernel object if earned; DDoS-isolation proof.
- **N7 - WireGuard + IPsec + QUIC/HTTP3 + multicast/IGMP + traceroute/ICMP polish.**
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
the TTL-increment traceroute + time-exceeded parsing is **N7**.

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
the Linux AF_UNIX personality (the next slice). **ICMPv6** echo and **full
traceroute** (the TTL-increment loop + time-exceeded parsing) are deferred to N7.
The next-hop choice today ARPs the destination directly (SLIRP proxy-ARPs
`10.0.2.0/24`); a real routing table (gateway for off-link) is a later refinement.

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
