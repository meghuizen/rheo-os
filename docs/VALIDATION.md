# Validation - Proving the Foundation Across All Profiles

**Status:** Draft v0.1. Extends ARCHITECTURE.md section 8 (which validates the
foundation's core mechanisms, P1-P12) to the full profile spectrum
(PROFILES.md). Two jobs: (1) show the foundation *covers* every profile - the
design claim; (2) give every profile concrete validation with targets, kill
thresholds, and honest baselines - the proof regime. A greenfield foundation
claiming this range earns it per profile, against the best incumbent for that
profile, or the claim is withdrawn.

## 1. The design claim: one closed foundation covers the spectrum

The strongest statement about the foundation is a coverage fact: **every
profile is a composition of the same ten kernel objects** (ARCHITECTURE.md 3).
Across the whole spectrum, the only new *subsystem* the range forced was power
(POWER.md) - and it composed from existing machinery (reservations, metering,
pressure events, Arch trait) rather than adding kernel objects. The object
model stayed closed under the widest scope pressure this project has applied
to it. That closure, not a feature list, is the evidence the foundation is
"powerful enough."

Coverage matrix - which primitives each profile stresses hardest:

| Profile | Primitives under maximum stress |
|---|---|
| Server / fleet | All ten, baseline |
| Database | Reservations (the contract), memory grants (no-OOM), durability classes + group commit, leases |
| Data warehouse | Typed memory (HBM/CXL tiers), DMA graphs, sealed Arrow buffers, storage pushdown |
| AI inference | Engines, dependency graphs (conditional edges), paged KV grants, sealed model objects |
| Container / OCI | Cells, capability minting rate, sealed image objects, K8s-edge state store |
| VM host | vCPU engines, nested-paging memory kinds, SR-IOV engine grants, attestation tiers |
| Internet exchange | NIC queue grants, steering, event stream at line rate, system-pool isolation |
| Firewall | WASM dataplane (3 tiers), capability-gated policy, per-cell buffer economics |
| Cloudflare-type edge | QUIC-as-native-queues, WASM cells, CID steering, DDoS pipeline, object-store cache |
| Embedded / IoT | Footprint of the core, hard-RT reservations, A/B images, attestation on small roots |
| Remote / low-power | Leases + partition tolerance, energy budgets (POWER.md), durability under power loss, HLC sync |
| Desktop (deferred) | Compositor sealed-buffer handoff, HID queues, suspend/resume, POSIX breadth |

Reading the matrix vertically: every kernel object appears under maximum
stress in at least two unrelated profiles - the same reuse signal the
governance rule treats as evidence of a right-sized primitive
(ARCHITECTURE.md 6).

**Standing rule:** if validating any profile ever requires a *new kernel
object* rather than new cells, drivers, or policy, that is a design event -
the admission rule (ARCHITECTURE.md 6) is invoked and this document and the
object model are revised together. The bet is that it will not happen; the
rule is what makes the bet falsifiable.

## 2. Validation principles (inherited and extended)

- **Same kill semantics as ARCHITECTURE.md 8.4:** a red cell re-examines the
  mechanism or withdraws the claim; benchmarks are never re-tuned until green.
- **Honest baselines per profile.** Tuned Linux is not always the right
  control: networking profiles are measured against DPDK/VPP-class stacks,
  containers against containerd/runc, VMs against KVM (QEMU and
  Firecracker-class), inference against vLLM-on-tuned-Linux, embedded against
  a Yocto/PREEMPT_RT-class Linux on identical boards. Beating stock anything
  proves nothing; each profile races the best incumbent for that job.
- **Battle-testing applies per profile** (ARCHITECTURE.md 8.5): each profile's
  validation ends with real workloads, chaos, and soak - not only the
  benchmark table below.
- **Validation is sequenced with delivery** (PROFILES.md 5): a profile's
  targets gate that profile's release, on the phase it ships in. Nothing here
  moves the early milestones.

## 3. Per-profile validation matrix

Numbering continues from P12 (ARCHITECTURE.md 8.4). Targets are initial and
will be recalibrated against measured baselines at each profile's start -
what is fixed is the *ratio to the named baseline* and the kill semantics.

### 3.1 Database OS

Baseline: the same DB engine on tuned Linux (io_uring, hugepages, pinned).

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P13 | Postgres (personality) TPC-C-class throughput | >= 90% of tuned-Linux | < 75% |
| P14 | p99 commit latency under mixed load | <= tuned-Linux p99 | > 1.5x |
| P15 | The database contract: host under memory/CPU pressure | DB reservation unaffected (< 2% p99 shift); no OOM event possible | any forced kill of a reserved cell |
| P16 | Pull-the-power durability drill | zero committed-transaction loss across 1000 cycles | any loss |

### 3.2 Data warehouse OS

Baseline: same query engine (DuckDB/ClickHouse-class via personality, and a
native-Arrow engine) on tuned Linux.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P17 | Parquet scan with storage/DPU pushdown | >= NVMe line rate delivered as needed columns; host CPU <= 50% of Linux baseline for same scan | no pushdown win at all |
| P18 | ClickBench-class query suite | >= 90% of tuned-Linux | < 70% |
| P19 | Cross-cell Arrow table handoff | zero copies in the flow trace (seal + capability only) | any hidden memcpy on the path |

### 3.3 AI inference (extends M5)

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P20 | Cold model load, NVMe->HBM (70B-class) | >= 80% device line rate (P7) and end-to-end < 2x theoretical minimum | > 4x |
| P21 | Multi-tenant GPU: noisy neighbor on adjacent partition | < 5% p99 token-latency impact | > 15% |
| P22 | Continuous batching via completion windows | throughput within 15% of vLLM control at equal p99 (M5 gate restated) | > 30% worse |

### 3.4 Container / OCI foundation

Baseline: containerd/runc on tuned Linux; K8s conformance via the edge.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P23 | Cold start, OCI image -> running cell | <= containerd baseline; stretch: 5x faster (no overlayfs/CNI path) | > 2x slower |
| P24 | Density: idle-but-live service cells per host | >= 2x containerd density at equal RAM | < 1x |
| P25 | K8s conformance subset through the edge | 100% of the defined supported-API subset; subset covers the top workload patterns | core Deployment/Service/ConfigMap patterns failing |

### 3.5 VM host OS

Baseline: KVM (QEMU and Firecracker-class) on the same hardware.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P26 | Linux guest boot + virtio net/blk throughput | >= 90% of KVM baseline | < 70% |
| P27 | SR-IOV passthrough NIC in guest | >= 95% of bare-metal device rate | < 85% |
| P28 | Confidential VM attest-and-launch flow | end-to-end verifiable chain, launch overhead < 2x plain VM | broken chain |

### 3.6 Internet exchange

Baseline: VPP/DPDK l3fwd-class on identical NIC/CPU.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P29 | L3 forwarding per core (64B packets) | >= 85% of DPDK-class baseline | < 60% |
| P30 | Route-server scale | full-table (1M+ routes) convergence and steering-table update without data-plane stall | data-plane stalls on control churn |
| P31 | Flood at one peer port | zero measurable impact on other ports (P9 generalized) | > 5% p99 impact |

### 3.7 Firewall appliance

Baseline: VPP/nftables-tuned Linux.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P32 | Stateful session capacity | >= 10M concurrent sessions/host in bounded memory (sketch+table design) | memory growth unbounded under churn |
| P33 | Rule-set compile-to-dataplane | 100k-rule policy compiled to tiers with per-packet cost flat vs 1k rules | per-packet cost scaling with rule count |
| P34 | Policy update under load | atomic, zero dropped-but-should-pass packets | inconsistent windows |

### 3.8 Cloudflare-type edge

Baseline: nginx/quiche-class TLS/QUIC termination on tuned Linux; Workers-class
runtime for WASM comparison.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P35 | TLS1.3/QUIC handshakes per core | >= 90% of baseline (with inline NIC crypto: exceed it) | < 70% |
| P36 | WASM edge-cell cold start | < 1 ms | > 10 ms |
| P37 | Cache hit serving from object store | NIC line rate, zero-copy path verified in trace | copies on hit path |
| P38 | Anycast/CID failover | connection survival across steering re-route (QUIC migration) | dropped connections on planned failover |

### 3.9 Embedded / IoT (MMU-class)

Baseline: Yocto + PREEMPT_RT Linux on the identical board.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P39 | Footprint | kernel + minimal cell set: <= 16 MB RAM floor, <= 4 MB image; boot-to-workload < 500 ms | > 4x any bound |
| P40 | Hard-RT jitter on the board (P12 tightened) | worst-case scheduling jitter <= PREEMPT_RT baseline on same board | > 2x worse |
| P41 | OTA A/B update with power-loss injection | 1000 update cycles with random power cuts: zero bricked, always boots a valid image | any brick |
| P42 | Attestation on small roots (add-on TPM / DICE) | full identity chain on the embedded profile's reduced floor | chain unverifiable |

### 3.10 Remote / low-power

Baseline: same board running tuned embedded Linux; energy measured at the
wall/battery, not estimated.

| # | Hypothesis | Target | Kill |
|---|---|---|---|
| P43 | Idle draw + wakeups/sec (tickless + gating; latency core targets zero timer wakeups at idle; deep C-state residency measured) | <= Linux baseline on same board; stretch 30% below | > 20% above, or idle wakeups exceed the profile's budget |
| P44 | Energy-budget adherence | 72h on a fixed joule budget: workload paced, budget never overrun (POWER.md 3) | overrun or crash |
| P45 | Brownout drill | staged energy-pressure -> graceful shed -> safe halt; zero data corruption on 500 random power pulls | corruption |
| P46 | Disconnected autonomy + sync | 7-day partition: local operation throughout; reconnect syncs cleanly (HLC merge, no manual repair) | divergence needing manual repair |

### 3.11 Desktop (deferred - smoke level only)

No P-numbers yet; the profile is the longest arc (PROFILES.md 3.2). Smoke
gate when attempted: compositor + a Vulkan app + POSIX terminal, input-to-
photon latency competitive with a Wayland compositor, suspend/resume 100-cycle
soak with mandatory DRBG reseed. Full validation defined when the profile is
scheduled.

## 4. Cross-profile validation (the foundation itself, again)

Because all profiles share one core, three suites run *identically across
every profile's reference hardware*, catching foundation regressions where
profiles diverge:

- **The invariant suite:** capability isolation (proof-adjacent tests),
  seal/TOCTOU, no-OOM contract, lease fencing, DRBG reseed-on-restore -
  identical pass criteria from IX box to solar node.
- **The three numbers** (grant check, queue round trip, context switch) -
  tracked per platform; a profile's hardware may be slower, but the *shape*
  (batching amortization, tickless jitter) must hold everywhere.
- **Chaos + soak** (ARCHITECTURE.md 8.5) parameterized per profile: partitions
  for the fleet, power pulls for embedded/remote, flood for the network
  appliances, tenant churn for the container host.

## 5. Sequencing and gates

Validation attaches to the delivery phases (PROFILES.md 5):

1. Phase 1 (server, database, warehouse, container, networking appliances):
   P13-P19, P23-P25, P29-P38 come online with M2-M4 hardware.
2. Phase 2 (AI at M5, VM host): P20-P22, P26-P28.
3. Phase 3 (embedded/IoT, remote/low-power): P39-P46, gated on POWER.md and
   the relaxed floor.
4. Desktop: smoke only, later.

A profile ships when its table is green *and* its battle-testing regime
(real workloads, chaos, soak) has run - the same two-part bar as the
foundation itself (PRODUCTION.md 7).

## 6. Honest notes

- The initial numeric targets above are engineering estimates; the committed
  part is the baseline choice, the ratio discipline, and the kill semantics.
  First measurement on each profile's reference hardware recalibrates numbers
  *once*, before the gate is armed - never after a red result.
- Some baselines are moving targets (vLLM, VPP, Firecracker improve); gates
  compare against the baseline version pinned at profile start, with an
  annual re-pin.
- The networking-appliance targets (P29-P38) are the boldest, because
  DPDK/VPP-class stacks are extremely mature; they are also where the design's
  native model is closest to the incumbent's architecture, which is why the
  targets are ratios near parity rather than claimed wins. Wins, where they
  come, should come from isolation and operability at equal speed - and the
  table is designed to detect if even parity fails.
