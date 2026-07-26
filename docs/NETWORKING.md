# Networking

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.7.

Position: the kernel owns **queue plumbing, flow steering, and grant checks -
and no network stack**. Protocols are libraries in cells or offloads on the
NIC/DPU. The design goal is that a packet nobody holds a grant for costs
approximately nothing, and that a flood against one tenant cannot perturb
another.

## 0. What is built (Phase G): the NIC data path, raw frames

The **NIC driver + a raw-frame async path** are real; the **IP/TCP/QUIC stack
stays deferred as a service** (sections 2-3), exactly as the position above
requires - the kernel owns the queue plumbing, not the protocols.

- A hand-written **virtio-net driver** (`kernel/src/hw/virtio_net.rs`), mirroring
  the virtio-blk driver over the same **two transports** - virtio-mmio on
  arm/riscv `virt`, virtio-pci on x86-64 q35 (through the `VIRTIO_PCI_CAP_PCI_CFG`
  config tunnel, no BAR mapping). Reset + minimal feature negotiation
  (`VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MAC`; no mergeable-rx-buffers or
  checksum/GSO offload), an RX and a TX **split virtqueue**, and the 12-byte v1
  `virtio_net_hdr`. DMA uses **physical** addresses (`virt_to_phys`) since the
  kernel moved to the higher half. Transmit is polled; **receive is now
  interrupt-driven on riscv/arm** (rheo-net N2d, docs/NETSTACK.md §16 - x86-64's
  virtio-pci NIC has no usable interrupt line here and keeps a kernel-side poll).
- librheo's **`net`** is now a real async surface: `mac()`, `send(frame)`,
  `try_recv(buf)` over three queue opcodes (`OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC`)
  bridged to the driver in `kernel_process`, completing with the strand token -
  the same async model as the Phase B `io` opcodes - plus `recv`/`recv_timeout`,
  which **park** on the reactor's network slot and are woken by the NIC's receive
  interrupt through `SYS_WAIT_NET` (rheo-net N2d, docs/NETSTACK.md §16). `connect`/`listen` stay
  `Unsupported` stubs: a socket/IP/TCP layer is a **service** (section 2).
- Proof: the `librheonet` test kernel (all three ISAs) - a librheo cell asks the
  NIC for its MAC, sends a **broadcast ARP request** for the SLIRP gateway
  `10.0.2.2`, and **receives SLIRP's ARP reply** (a real, deterministic,
  network-free RX proof over QEMU `-netdev user`), asserting the reply's ethertype
  + opcode + sender IP and exiting `0x42`.
- Deferred (documented): the full transport stack (IP/ARP-cache/TCP/QUIC/TLS as a
  library in a cell, section 2), a first-class socket `ObjectKind` + steering-table
  grants (section 1), header/payload split (section 1), and everything in sections
  4-7 (eBPF dataplane, DDoS staging, DPU offload). The **device RX interrupt** is
  **done** for riscv/arm (rheo-net N2d, docs/NETSTACK.md §16); x86-64 MSI-X through
  the PCI config tunnel, interrupt coalescing, and zero-copy receive remain.

> **Service-cell update (N4a, docs/NETSTACK.md §17):** the doctrine of this
> document - the stack is a **userspace service cell**, the kernel owns only queue
> plumbing and grant checks (§§1, 5-9) - now has its serving substrate. A service
> cell holds **one cross-cell channel end per client** and runs **one strand per
> client**, so a single network service cell serves many client cells concurrently
> (proven with 3, all three ISAs). It adds **no kernel object**: a per-cell channel
> *table* (`MAX_CELL_CHANNELS = 4`), a channel-slot selector on `SYS_SPAWN`, and
> `SYS_YIELD` (a round-robin generalisation of `SYS_SWITCH`'s `cur^1` hand-off,
> needed because an XOR cannot reach client 3 from client 2) - all mechanism over the
> existing Cell + QueuePair objects, all documented in NETSTACK.md §17. Honest:
> **concurrent, not parallel** (one CPU, cooperative - SMP is task #27), fan-out is
> parent-shaped (the service spawns its clients; a name-based rendezvous for
> unrelated cells is a documented follow-on), and the wire protocol is word-wide
> today (bigger requests ride a shared sealed grant). This is what N4b (the
> remote-INET bridge for unmodified Linux binaries) and N5 (app protocols) build on.

> **Security transports update (N3a/N3b, docs/NETSTACK.md §14-15):** the crypto
> primitive layer (N3a - ChaCha20-Poly1305 + RustCrypto SHA-2/HKDF/X25519/
> Ed25519/AES-GCM) and a from-scratch **TLS 1.3** handshake + record layer +
> minimal X.509 (N3b) are built and proven byte-for-byte against the RFC 8448
> known-answer trace. Both are userspace-only, feature-gated, and add no kernel
> object - consistent with §9 (TLS lives in userspace; keys are non-extractable
> handles, the seam for the per-queue inline-crypto offload of §9).

## 0a. What is built (rheo-net Phase N1a-N1e + N2a/N2b): the L2/L3/L4 core + caching DNS + traceroute + native TCP + congestion control

The **greenfield network stack** begins here as **portable userspace** - a new
`net/` workspace crate (`no_std` + alloc, no per-ISA code) built for the three
bare targets as a loaded ELF cell, riding ON the Phase G raw-frame path. Its full
architecture, crypto posture, and the N1-N8 roadmap are **docs/NETSTACK.md**.

Phase N1a ships the L2/L3 core - `eth` (Ethernet II parse/build, zero-copy views),
`arp` (request/reply + an IP->MAC cache + an async `resolve` over `librheo::net`),
and `ip` (IPv4 + IPv6 header parse/build + the RFC 1071 ones-complement Internet
checksum, a reusable accumulator UDP/ICMP inherit). It adds **no kernel object and
no per-ISA code** - it is pure parsing over the existing `OP_NET_*` queue path.

Proof: the `netcore` test kernel (all three ISAs, same SLIRP + virtio-net wiring
as `librheonet`) - a cell reads the NIC MAC, **resolves the SLIRP gateway
`10.0.2.2` through `net::arp`** (the ARP round trip now runs through the stack's
`eth`/`arp` layers, not `librheonet`'s hand-built frame), validates the checksum
against a known value (`0xB861`), round-trips an IPv4 header build/parse/validate
(a flipped byte fails), and round-trips an IPv6 header - exiting `0x42`.

**Phase N1b (L4)** adds `udp` (datagram build/parse + the pseudo-header checksum,
reusing N1a's `Checksum` accumulator, + an async `UdpEndpoint`), `icmp` (ICMPv4
echo/ping + the IPv4 TTL hook for a later traceroute), and `wire` (the shared
eth/ip framing). Proof: the `netl4` test kernel (all three ISAs) sends a real DNS
query over **UDP** to SLIRP's built-in responder at `10.0.2.3:53` and an **ICMP
echo** to the gateway `10.0.2.2`, asserting the UDP checksum validates + the
transaction id echoes, the ping reply type/id/seq match, and two known-good
checksum oracles (`0x6D45` UDP, `0xFFE0` ICMP) - exiting `0x42`, network-free.

**Phase N1c** adds `dns` - a from-scratch **caching DNS client**: a full message
codec (A/AAAA/CNAME + loop-bounded name-compression pointers), an async caching
`Resolver` over `udp`, an **LRU + TTL cache**, a **blocklist** (a from-scratch
hash set + wildcard suffixes, designed for huge lists via a grant-backed arena),
and configurable resolvers + a static hosts table. Proof: the `netdns` test kernel
(all three ISAs) asserts, **network-free**, a compressed-response parse oracle +
pointer-loop safety, and hosts/blocklist/cache resolutions that each send **zero**
network queries (a query counter is the evidence), plus a **bonus live** resolve of
`example.com` over SLIRP's DNS (`10.0.2.3`) that asserts only structure and
tolerates a timeout where there is no outbound DNS - exiting `0x42`. Negative
caching is deferred (documented).

**Phase N1e** makes TTL (IPv4) and Hop Limit (IPv6) **first-class** (default 64,
built + parsed), adds the forwarding-plane `ip::decrement_ttl`/`decrement_hop_limit`
primitives (the router/firewall path - decrement, recompute the IPv4 checksum,
drop + signal Time Exceeded at zero), ICMP **Time Exceeded** (v4 type 11 +
the v6 type 3 codec), and a real **traceroute** state machine (`net::trace`,
ICMP-echo probes correlated by sequence number). Proof: the `nettrace` test kernel
(all three ISAs) asserts, **network-free**, the TTL/hop-limit round-trips, the
decrement primitive (checksum oracle `0xB961`, drop signal at TTL 0/1), the Time
Exceeded oracles (`0xF4FF` v4, `0x1936` v6), and the **traceroute state machine
fed synthetic responses** reconstructing an exact 4-hop path (multi-hop discovery
without real routers), plus a **bonus live** 1-hop trace to the gateway `10.0.2.2`
that tolerates a timeout - exiting `0x42`. Still deferred: the *live* ICMPv6 path +
IGMP/MLD are N7 (docs/NETSTACK.md 5-9).

**Phase N2a** adds the native **TCP** transport: `net::tcp` (the RFC 793 state
machine - handshake, sliding-window flow control, RFC 6298 RTO/RTT + Karn,
cumulative-ack retransmission, FIN teardown + TIME-WAIT, the TCP checksum, and a
`CongestionControl` trait seam for the N2b CUBIC/BBR drop-in) and `net::timer` (a
timer wheel multiplexing many logical timers - per-connection RTO / TIME-WAIT -
onto the reactor's **single** one-shot deadline, pure userspace, no ABI change).
Proof: the `nettcp` test kernel (all three ISAs), **deterministic + network-free** -
two TCP endpoints in one cell over a virtual link drive the full lifecycle
(three-way handshake, bidirectional data with exact bytes, a **dropped segment
recovered by RTO**, clean teardown to CLOSED/TIME-WAIT), plus the checksum/segment
oracles (`0x613C`) and the timer-wheel multiplex - exiting `0x42`. A **live** TCP
handshake to SLIRP is **skipped with reason** (SLIRP has no TCP responder to make
it deterministic).

**Phase N2b** fills the seam with real **congestion control**: `net::cc` -
**Reno** (RFC 5681: slow start, AIMD, fast retransmit / fast recovery on 3 dup ACKs,
RTO slow-start restart) and **CUBIC** (RFC 8312: the cubic window `W(t)` in
**integer / fixed-point** math with an integer cube root, plus the TCP-friendly
region), both drop-in `CongestionControl` impls wired into the send window; `net::tcp`
gained dup-ACK detection + fast-retransmit-before-RTO. Proof: the `nettcpcc` test
kernel (all three ISAs), **deterministic + network-free** - integer cwnd trajectories
(slow start, AIMD, fast retransmit/recovery, RTO, the CUBIC `W(t)` shape) pinned
against precomputed oracles + a real `Connection<Reno>` fast-retransmit scenario over
the in-cell virtual link, exiting `0x42`. A **live** TCP handshake is again
**skipped with reason**. The smoltcp cell + the sharded transport + a live handshake
are **N2c**; SACK/window-scaling/ECN, NewReno partial-ACK recovery, CUBIC
HyStart/fast-convergence, and BBR are deferred (docs/NETSTACK.md 11-12).

## 1. NIC queues are the primitive

- A network grant = a set of hardware RX/TX queue pairs, IOMMU-mapped into
  the owning cell, plus entries in the NIC's flow-steering tables. The kernel
  programs steering (5-tuple / VLAN / MAC / QUIC connection-ID -> queue) and
  leaves the data path. Packets DMA straight into the cell's pre-posted
  arena buffers.
- This is DPDK/ef_vi as the *native* model, with the isolation DPDK never
  had: IOMMU per queue plus metered grants. Poll-vs-interrupt is a grant
  attribute per queue.
- **Header/payload split** lands payloads in 2 MB-aligned arena pages and
  headers elsewhere, so a received payload is directly mappable into a GPU -
  the network-to-HBM zero-copy path (IO.md 3, AI-ARCHITECTURE.md).

## 2. Transports are libraries

- TCP, QUIC, and TLS live in userspace. **One blessed Rust transport
  library** ships as a system cell others link or proxy through; third-party
  stacks are allowed, sandboxed, and unsupported - the same posture as
  third-party crypto.
- Per-cell transport state means no global lock contention and no
  shared-stack blast radius; a buggy stack burns only its cell's budget.
- Congestion control is pluggable **per connection by policy grant**: BBRv3
  default on the WAN, CUBIC for compat, DCTCP/ECN inside the fabric where
  switches mark.
- Listening "ports" are steering-table grants: who may bind 443 is
  capability minting, and port-hijack/SO_REUSEPORT-squatting classes become
  policy questions.

## 3. QUIC first, TCP as compat

QUIC matches this design's shape better than TCP ever fit Linux:

- Userspace transport (it never wanted the kernel), connection-ID steering
  (migration and anycast become NIC-table operations), 0-RTT with
  anti-replay, and streams that **terminate as native queue pairs** - so the
  edge-gateway translation is thin.
- **Encryption performance:** per-packet AEAD runs either as good software
  (AES-GCM at multi-GB/s/core via VAES/AVX-512 or ARM CE, dispatched on the
  measured Arch crypto path) or as **inline NIC crypto offload per queue**.
  Because cells own NIC queues directly, offloaded QUIC/TLS drops in without
  a kTLS-style special kernel path; keys are capabilities programmed into the
  queue and never readable back.
- TLS 1.3 over TCP gets the same per-queue inline-crypto treatment; handshake
  asymmetric crypto uses hardware acceleration where present; resumption
  state is cell-local under identity-plane keys.

## 4. The eBPF role: verified WASM dataplane

Three attachment tiers, each with a different power/cost contract, all
capability-gated and cycle-budgeted:

1. **NIC/DPU pipeline** - compiled to match-action tables or DPU cores
   (a P4-shaped subset; the verifier tells you what lowers to a TCAM).
2. **Host pre-steering cores** - a stateless XDP-equivalent before per-cell
   steering: programmable drop/rewrite/sample at line rate.
3. **In-cell hooks** - full expressiveness inside your own transport library,
   your budget.

Same verified-WASM machinery as the observability probes (TOOLING.md); a
program that cannot crash, block, or exceed its declared budget, with an
audit trail for attachment - versus eBPF's root-or-nothing and verifier-
escape CVEs.

## 5. DDoS: drop-cost inversion at IX/edge scale

The invariant: cost-to-drop must be orders of magnitude below cost-to-serve,
and drops must not perturb served traffic. Stages, cheapest first:

1. **NIC hardware:** steering misses and explicit drop rules die at the NIC -
   zero host cycles, counted in hardware. Blocklists/rate-limits compiled
   into NIC tables hold hundreds of thousands to millions of entries on
   modern NICs/DPUs.
2. **Stateless pre-steering cores** (a system-pool reservation, so attack
   load saturates *these*, never workload cores): SYN cookies, **QUIC Retry**
   address validation (no server state until the client proves reachability),
   malformed-packet drops, and per-source token buckets in a **count-min
   sketch** - bounded memory under any attack, no per-flow allocation an
   attacker can inflate.
3. **Buffer economics:** RX buffers come from the owning cell's arena, so a
   flood aimed at one service exhausts *that queue's* buffers, the NIC drops
   in hardware for that queue, counters tick, neighbors are untouched. There
   is no shared skbuff pool to exhaust and no softirq storm stealing
   everyone's CPU - the classic Linux collapse mode.

Cost of a dropped packet: a hardware counter increment. Cost of an unhandled
packet (nothing listening): it never crosses the PCIe bus.

Fleet dimension: attack telemetry (sketch summaries, drop counters) flows
through the observability plane; mitigation programs push cluster-wide
through the state store like any desired-state object; anycast + CID steering
scales scrubbing horizontally with the normal placement machinery.

## 6. East-west vs north-south

- **East-west (internal):** capability-gated RDMA/queue-pairs under mTLS
  identity from the cluster design. Internal services have no IP surface to
  scan or flood - there is nothing to address without a grant. This asymmetry
  is itself the primary internal defense.
- **North-south (internet edge):** the TCP/QUIC/DDoS machinery is
  fundamentally an edge-cell concern, sized for the gateway cells that hold
  internet-facing grants.
- Network policy is capability issuance, not runtime firewalling
  (SECURITY-IDENTITY.md 6).

## 7. DPU offload trajectory

BlueField-3+/Pensando-class DPUs progressively run the whole edge:
pre-steering, inline TLS/QUIC crypto, and eventually full transport
termination on DPU cores executing the same `no_std` Rust as the host - one
of the payoffs of one language from kernel to SmartNIC. The grant system
exposes what each NIC/DPU generation actually offers; the design never
pretends the hardware feature matrix is uniform.

## 8. Honest costs

- Owning a transport library forever means meeting pathological middleboxes
  Linux met decades ago; mitigated by one blessed library plus an aggressive
  interop corpus (risk #3 in ARCHITECTURE.md 8.7).
- Dedicated poll cores burn capacity at idle; the pre-steering stage scales
  its core count with load via elastic reservations, but there is a floor.
- Per-queue buffer isolation trades memory for isolation; million-connection
  edges need connection-to-queue multiplexing in the transport library
  because queues are a steering granularity, not a per-connection promise.
- NIC offload heterogeneity is real; the feature matrix differs per hardware
  generation and the grant layer surfaces it honestly (typed-hardware
  doctrine, again).
