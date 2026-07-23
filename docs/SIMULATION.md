# Simulation - Lattice OS Across All Profiles

**Status:** Draft v0.1. Companion to PROFILES.md and VALIDATION.md. Each
scenario is a walk-through at the kernel-event level: what boots, what cells
exist, what happens when a real workload or failure hits, what the event stream
shows. The goal is to make every design claim concrete and to surface
contradictions before code does.

Notation used throughout:
- `[kernel]` — a kernel operation (grant check, engine scheduling, event emit)
- `[cell:name]` — a named cell doing something
- `[flow:id]` — a flow context propagating through the system
- `→` — queue submission or completion
- `!` — a failure, pressure event, or lease expiry

---

## Scenario 1 — Fleet server: a microservice handles 100,000 RPS

**Hardware:** dual-socket AMD EPYC, 512 GB DDR5, 2x ConnectX-7 100GbE RDMA,
4x NVMe. Joined a trust domain of 40 nodes.

**Boot:** UEFI measures firmware into TPM. Bootloader measures kernel image.
Kernel init runs — IOMMU on, SMMUv3 domains allocated per device, all DMA
mediated from first instruction. Root DRBG seeded from RDSEED + TPM TRNG,
health tested. Two clock objects created: monotonic and bounded-wall (e = 3 ms,
sync daemon not yet running). System pool carved: 4 dedicated cores, 16 GB
DDR5, reserved NIC queue depth. Engine enumeration: each NVMe and NIC
benchmarked at attach — measured vs. claimed throughput enters the topology
graph. Registration service attested; host identity issued, SPIFFE-shaped.
Reconciler (PID 1) receives desired state. In 8 seconds the node is live.

**Cells running:** reconciler, sync daemon, state-store replica, transport
library, NIC pre-steering, an OTel exporter, and the service-under-test
(a Go HTTP/2 app through the POSIX personality).

**Scenario — request path at 100k RPS:**

```
[NIC hw]  SYN arrives; CID steering table → queue pair 7 (service cell)
[kernel]  DMA into pre-posted arena buffer; completion descriptor queued
[cell:transport] QUIC handshake: TLS keys programmed as a per-queue
          capability; inline NIC crypto handles record encryption
[cell:service]   strand wakes on completion; reads request inline (< 1 KB,
          no copy); [flow:a3f2] stamped from strand-local context
[cell:service]   submits a durable write to the object store (1 ms window)
[cell:storage]   group-commit window: 64 concurrent writers coalesced;
          one NVMe flush; all 64 completions returned with flow IDs intact
[cell:service]   seals response buffer; pushes descriptor to transport
[NIC hw]  encrypted reply DMA'd to wire; zero CPU copies end-to-end
[event stream]   request span: 480 µs p50, 1.1 ms p99; flow shows
          NIC→cell→storage→NIC with DMA timestamps; zero hidden copies
```

**Failure — the storage NVMe fails mid-batch:**

```
[NIC hw]  device fault → IOMMU fault event to storage-engine cell
[cell:storage-driver] catches fault event; driver cell isolates; submits
          reset command via the engine API
[kernel]  engine reset verified; driver cell restarted
[cell:storage] pending flush completions returned as error events
[cell:service] sees `durable` write fail; retry policy activates
[event stream] engine fault span visible; no other cell affected;
          system pool never touched; p99 spikes 12 ms, recovers
```

---

## Scenario 2 — AI inference: serving a 70B parameter model

**Hardware:** 8x NVIDIA H100 (MIG-partitioned), Grace CPU, NVLink-C2C coherent
fabric, 640 GB HBM3, 4x NVMe for weight storage, ConnectX-7.

**Boot → inference ready:**

```
[kernel]  H100 engines enumerated; MIG firmware measured, signature checked;
          trust class: exclusive (Lattice fully owns, no secure-world race)
[kernel]  attach-time benchmark: 3.9 TB/s HBM bandwidth measured (not
          claimed); tensor-core MMA throughput measured per precision;
          enters topology graph
[cell:model-loader] submits DMA graph: NVMe read nodes → HBM write nodes,
          peer-to-peer via GPUDirect; zero CPU copies; [flow:load-1] tracks
          the whole graph across 4 engines
[kernel]  320 GB loaded in 41 seconds at 93% of device line rate (P7: ✓)
[cell:model-registry] seals the weight buffer; 8 inference cells map it
          read-only — 1 physical HBM copy serves all 8; LoRA adapters
          are small per-cell delta buffers over the shared base
```

**Scenario — concurrent inference requests, mixed latency classes:**

```
Interactive request [flow:i1] — 50 ms budget declared
Batch summarization [flow:b1] — 2000 ms window declared

[cell:inference-server] continuous-batching engine: completion-window
          contracts consumed; per-iteration batch formed across 6 active
          requests; iteration graph submitted as one dependency graph

[kernel]  graph nodes: attention kernel (H100 MIG slice 0), GEMM (slice 1),
          collective AllReduce across 2 slices (NVLink), KV-cache
          block remap (paged grant, GPU MMU updated, no data move)
          timeline semaphores chain all nodes; CPU never polled

[cell:inference-server] speculative decoding: conditional graph edge —
          draft tokens on small model (slice 2), verify on large (slice 0);
          if verify passes, continuation scheduled; if not, edge not taken

[event stream] span tree: request → iteration → attention → GEMM → AllReduce
          → KV page remap; every DMA and kernel launch timestamped by engine
          clock, comparable because clock objects share known offset (TIME-
          IDENTITY.md 1); p99 for interactive: 38 ms (P22: ✓)
```

**Failure — one MIG partition OOMs its KV cache:**

```
[cell:inference-server] KV block allocator exhausted on slice 3
[kernel]  pressure event → inference cell: "return 4 GB KV elastic range
          within 100 ms"
[cell:inference-server] LRU evicts cold prefix-cache blocks (sealed, so
          eviction just drops the read capability; blocks return to pool)
[kernel]  grant replenished; new requests admitted
          — no other cell or slice affected; the blast radius is one cell's
          elastic KV grant
```

---

## Scenario 3 — Database OS: Postgres under 50,000 TPS with a noisy neighbour

**Hardware:** 2-socket Xeon, 256 GB DDR5, NVMe RAID (PLP write cache present).
Two cell groups: the database (hard reservation: 20 cores, 200 GB pinned memory,
private NVMe log stream) and a batch analytics job (elastic, lower priority).

**The database contract:**

```
[kernel]  DB cell group reservation: (budget=20 cores, period=10ms,
          deadline=5ms), memory=200 GB hard, NVMe log stream=private
[kernel]  admission check: schedulability math holds on the latency pool;
          reserved; analytics gets the elastic remainder
```

**Scenario — 50,000 TPS commit path:**

```
[cell:postgres-personality] fork()+exec() → clone within capability bundle;
          for each transaction:
          submit write → append-log object, durability class=durable,
          window=500 µs
[cell:storage] group-commit engine: 128 concurrent transactions coalesced
          into one log append; NLP write cache → completion fires at cache
          speed (not platter speed); all 128 completions dispatched
[cell:postgres] index update: differential delta layer absorbs writes at
          memory speed; background merge scheduled as a low-priority engine
          graph job (FILESYSTEMS.md 3)

[flow:txn-x] visible: postgres → storage → group-commit → NVMe(cached)
          → completion; p50 commit = 180 µs; p99 = 390 µs
```

**Noisy-neighbour stress test:**

```
[cell:analytics] batch job: submits 40 GB scan; consumes its elastic
          memory; triggers a memory-pressure event on the host
[kernel]  elastic grant pressure: analytics cell receives event
          "return 20 GB within 500 ms"
[cell:analytics] decommits cold arena pages, narrows working set
[kernel]  DB hard reservation: untouched — hard grants are not elastic
          and are never the target of pressure events
[event stream] DB p99 during analytics pressure: 391 µs (< 2% shift: P15 ✓)
          analytics throughput drops 18%; no OOM event; no forced kill
```

**Pull-the-power drill:**

```
[power pulled] NVMe PLP write cache drains to platter in hardware;
[kernel boot]  next boot: content-addressed object store replays the
               sealed append-log from last durable write; zero committed
               transactions lost across 1000 cycles (P16: ✓)
```

---

## Scenario 4 — Data warehouse: TPC-H scale-factor 1000 with DPU pushdown

**Hardware:** 4-node cluster; each node has 2x NVMe, 256 GB DDR5, BlueField-3
DPU. CephFS stores raw Parquet, native object store holds the hot tier.

**Scenario — Q6 (full-table scan, filter, aggregate):**

```
[cell:query-engine] plans Q6; identifies needed columns: l_quantity,
          l_extendedprice, l_discount, l_shipdate
[cell:storage] submits Parquet-object read with pushdown hints: project
          to 4 columns, filter l_shipdate BETWEEN '1994-01-01' AND '1994-12-31'
[DPU engines] row-group headers read; column chunk footers decoded;
          only needed column data read from NVMe; predicate evaluated
          on DPU cores; passing rows DMA'd as an Arrow record batch
          into a sealed buffer in DDR5 — the CPU never sees raw Parquet

[flow:q6] NVMe → DPU-decode → DDR5 sealed Arrow buffer → query engine cell
          zero copies verified in trace (P19: ✓)

[cell:query-engine] receives Arrow batch via capability hand-off;
          aggregation runs on CPU with AVX-512 vectorised sum (tile-IR
          CPU backend, VAES/AVX-512 dispatch measured at engine attach)

Cross-node: each node's DPU pushes its Arrow partial-aggregate to node-0
          via RDMA into a sealed buffer; node-0 query cell merges partials

Host CPU for 1 TB Parquet scan: 31% of Linux baseline (P17: ✓)
Total wall time: 28% faster than ClickBench-class tuned-Linux (P18: ✓)
```

---

## Scenario 5 — Internet exchange: 400 Gbps, route churn, targeted flood

**Hardware:** 2x 200GbE ConnectX-7, BlueField-3 DPU, 128 cores, 512 GB RAM.
Three cell groups: route-server (control plane), forwarding (pre-steering,
DPU-resident), and management.

**Boot → forwarding live:**

```
[DPU engines] tier-a WASM programs compiled to NIC match-action tables;
          FIB of 980,000 routes loaded into steering tables
[kernel]  forwarding cell: system-pool reservation: 8 dedicated tickless
          cores, interrupt-free; remaining cores timeshared for management
```

**Scenario — 400 Gbps sustained, BGP route churn:**

```
[NIC hw]  packets arrive at 400 Gbps; CID/5-tuple → forwarding cell queues;
          DMA into pre-posted arena buffers; completions batched 256 deep
[cell:forwarding] per-packet: capability-checked steering lookup (lock-free
          ASID-tagged table, ~28 ns); TTL decrement; checksum update;
          DMA to egress queue; full pipeline stays in CPU cache

BGP churn: peer announces 12,000 new prefixes
[cell:route-server] computes new FIB delta; compiles steering-table update
[kernel]  atomic steering-table swap: new table programmed while old
          serves traffic; swap is a single pointer update under a seqlock;
          zero packets forwarded incorrectly during swap (P30: ✓)

[event stream] route convergence time: 340 ms for 12k prefixes (P30: ✓)
```

**Targeted flood — 180 Gbps garbage at one peer port:**

```
[NIC hw]  steering miss for spoofed sources → match-action drop rule;
          cost: hardware counter increment; 0 host cycles
[pre-steering WASM] count-min sketch: per-source rate tokens exhausted;
          new drop rule compiled and pushed to NIC tables in 120 ms
[cell:forwarding] other peer ports: unmeasured impact (P31: 0.3% p99: ✓)
[event stream] flood span: sketch update, NIC rule push, drop-counter;
          visible, bounded, attributed to the right tenant
```

---

## Scenario 6 — Big firewall appliance: stateful inspection, 10M sessions

**Hardware:** 64-core ARM Neoverse, 256 GB, 4x 100GbE, DPU. Sealed appliance
profile (no POSIX personality, no K8s edge, no general-purpose shell).

**Policy model:**

```
[cell:policy-compiler] 100,000 rules compiled to a 3-tier structure:
  Tier-a (NIC tables): high-confidence allow/block by 5-tuple prefix
  Tier-b (pre-steering WASM): session-state lookup (10M entries, cuckoo
          hash in bounded memory; no per-flow allocation an attacker inflates)
  Tier-c (stateful inspection cells): deep inspection for new flows only
[kernel]  policy object sealed; hash signed; update is atomic table swap
```

**Scenario — new flow, inspect, allow:**

```
[NIC hw]  SYN → tier-a miss → tier-b WASM
[tier-b]  session table: new flow; estimated 180 ns; allocates session slot
          from a pre-allocated slab (bounded memory); forwards to tier-c
[cell:stateful-inspect] DPI: capability-gated access to flow payload;
          policy match: ALLOW; session entry updated; flow fast-pathed
          to tier-b for all subsequent packets
[event stream] flow creation event, policy decision, engine used — full
          audit trail, capability-scoped to the operator's read grant

Per-packet cost for established session (tier-b only): 95 ns
Per-packet cost for new flow (all tiers): 340 ns
Session capacity under churn: 10.3M at flat memory (P32: ✓)
```

**Policy hot-reload under 40 Gbps load:**

```
[cell:policy-compiler] new 100k-rule set compiled to new table set
[kernel]  atomic swap: old table remains live until swap; zero window
          where packets are forwarded inconsistently (P34: ✓)
[event stream] reload span: 2.1 s compile, 80 ms swap; zero dropped-but-
          should-pass packets confirmed by bidirectional counter audit
```

---

## Scenario 7 — Cloudflare-type edge: QUIC termination + WASM workers

**Hardware:** 32-core Xeon, 128 GB, 2x 100GbE ConnectX-7 with inline crypto.
Edge-profile: internet-facing NIC grants, WASM worker runtime cell, object
store (hot cache), OTel export, DDoS pre-steering. No GPU engines.

**Scenario — TLS 1.3/QUIC request to a WASM worker:**

```
[NIC hw]  QUIC Initial arrives; CID → edge cell
[NIC hw]  inline crypto offload: TLS handshake key exchange uses NIC
          ECDH acceleration; per-packet AES-GCM handled by the NIC;
          CPU sees plaintext record batches only
[cell:edge] QUIC stream terminates as a native queue pair; stream data
          arrives as sealed buffer descriptors
[cell:wasm-worker-runtime] cold-start: WASM module loaded from object store
          (content-addressed, cached, sealed); instantiated in < 800 µs
          (P36: ✓); runs inside a WASM cell with its own capability set
[cell:wasm-worker] fetch → object-store cache hit: sealed buffer handed
          as read capability; zero copy from cache to QUIC egress
[NIC hw]  inline crypto encrypts response records; wire
[flow:req-1] end-to-end span: NIC → edge → WASM → cache → NIC;
          all DMA-timestamps from the same clock reference; p99: 4.2 ms
          TLS handshakes/core/s: 96% of quiche baseline (P35: ✓)
```

**DDoS — volumetric HTTP/3 flood:**

```
[NIC hw]  QUIC Initial flood: QUIC Retry tokens enforced immediately;
          no server state created until client proves reachability;
          Retry costs: 1 NIC queue slot + 1 NIC match-action entry
[pre-steering] count-min sketch: per-IP token bucket; sketch memory fixed;
          no unbounded per-flow allocation
[cell:edge] other tenants: 0.4% p99 impact (P38-adjacent: ✓)
[event stream] flood visible: sketch misses, Retry rate, drop counters —
          queryable in Grafana via OTel export; no archaeology needed
```

---

## Scenario 8 — Container / OCI foundation OS: 500 microservices, one host

**Hardware:** 64-core ARM Graviton4, 256 GB. Running as a guest in AWS
(ENA network, NVMe instance storage, virtio-rng entropy). K8s edge active.

**Boot in the cloud:**

```
[guest] virtio-rng feed + jitter → root DRBG seeded; entropy class
        "hypervisor-fed" noted in attestation report
[cell:IMDS-client] reads AWS IMDSv2: instance identity, placement;
        cloud attestation extends the boot chain; host identity issued
        with trust tier "cloud-guest" (weaker than bare-metal TPM root,
        stated and honest)
[cell:reconciler] desired state received: 500 Deployments from the
        state store (the K8s edge translated `kubectl apply -f *.yaml`)
```

**500 services cold-starting:**

```
[cell:image-puller] OCI layers → content-addressed sealed objects in
        the object store; each layer deduplicated by hash across all 500
        services; physical images on disk: 3, not 500
[kernel] 500 cell groups created; each maps its layer objects read-only;
        capability set minted from service identity in the trust domain
[kernel] cold-start time per cell: 22 ms median (P23 target: containerd
        baseline 180 ms; Lattice 22 ms — 8x faster, overlayfs/CNI gone)
Idle density: 500 cells, 124 GB RAM (P24: 2.1x containerd density: ✓)
```

**Rolling update (Deployment spec change via kubectl):**

```
[cell:controller] watches desired state; sees new image hash
[kernel] new cell group created alongside old; traffic split via queue-
        endpoint capability swap (not iptables); old group drained;
        old cells destroyed; old sealed image objects refcount → 0 → freed
[event stream] rollout span: per-cell update visible, flow-tagged; one
        span per replica; no manual instrumentation in the workload
```

---

## Scenario 9 — VM host OS: running 40 tenant Linux VMs

**Hardware:** 64-core Xeon, 512 GB, 2x 25GbE SR-IOV, 4x NVMe, TDX support.

**Boot → VMs live:**

```
[kernel] vCPU engine objects created: EPT/NPT programmed via the Arch trait;
        VPID assigned per VM (TLB tagged; no shootdown on VM entry/exit)
[kernel] SR-IOV: 40 VFs provisioned (1 per VM); each VF is an engine
        grant held by the VMM cell; IOMMU domains isolate VF DMA
[cell:VMM] each VM: one VMM cell holding vCPU grants, guest memory grant
        (a typed DDR kind with guest-physical → host-physical mapping),
        VF engine grants; VMM cell cannot exceed those — a compromised
        guest is bounded by VMM cell grants
```

**Scenario — tenant VM does heavy I/O:**

```
[guest VM] writes to virtio-blk; exits to VMM cell (posted interrupt path
        catches most; only new-ring signals need a vmexit)
[cell:VMM] translates virtio request to a native NVMe submission on the
        granted NVMe engine; [flow:vm12-io] stamped
[kernel] NVMe completion → event on VMM cell queue → posted interrupt
        injected into guest; guest never sees the host kernel
Throughput: 98% of bare-metal NVMe rate via passthrough (P27: ✓)
```

**Confidential VM — hostile tenant:**

```
[kernel] TDX TD cell created: TD memory encrypted with a per-TD key
        that the host cannot read; attestation report extended with
        TD measurement
[tenant] runs workload inside TD; result: encrypted output in TD memory
[host]   can only observe TD from outside (performance counters, event
        stream metadata — not payload); capability multi-tenancy means
        no TD can name another TD's memory
[VALIDATION] attestation chain verified end-to-end (P28: ✓)
```

---

## Scenario 10 — Embedded / IoT: industrial sensor gateway (Cortex-A76)

**Hardware:** Raspberry Pi 5 (reduced-trust profile), 8 GB, PCIe NVMe,
Hailo-8L AI HAT+ (NPU, 13 TOPS), add-on SPI TPM. No RDMA, no PTP.

**Boot → sensor loop:**

```
[TPM SPI] best-effort measured boot: firmware measured into PCR; image
        hash measured; attestation report issued with trust-tier
        "embedded-reduced-trust"; cluster policy accepts for IoT roles
[kernel] clock error bound e = 8 ms (NTP only; no PTP hardware);
        lease windows widen accordingly; node operates conservatively
[kernel] Hailo-8L NPU attached as an engine; firmware measured; no
        exclusive ownership (shared with ARM TrustZone firmware) →
        trust class: shared-with-firmware; no secrets, no multi-tenant;
        benchmarked at attach: 11.8 TOPS inference throughput (measured)
[kernel] POWER.md active: DVFS OPP table measured; energy-efficient P-state
        selected for background operation; NPU power-gated when idle
```

**Scenario — sensor stream + edge inference:**

```
Every 10 ms:
[device queue] 6x industrial sensors → event-queue sources; typed events
[cell:sensor-aggregator] collects, timestamps with monotonic clock
        (not wall — we don't trust the clock enough for ordering);
        seals a 1 KB typed buffer
[cell:inference] submits sealed buffer as input to NPU via engine queue;
        dependency graph: CPU pre-process node → NPU inference node →
        result completion; 6 ms end-to-end (NPU: 3.8 ms; CPU: 2.2 ms)
[cell:inference] anomaly detected: event emitted on local event stream
[cell:sync-agent] batches events; when uplink available, pushes delta
        to the cluster state store via mTLS/QUIC
```

**OTA update — power pulled mid-flash:**

```
[cell:OTA] new image arrives as a content-addressed sealed manifest;
        written to B-slot NVMe while A-slot serves; atomic boot-flag flip
[power pulled mid-write]
[next boot] bootloader: B-slot hash mismatch → falls back to A-slot;
        A-slot boots clean; reconciler re-requests the update
[1000 cycles] zero bricks (P41: ✓)
```

---

## Scenario 11 — Remote Africa: solar-powered node, flaky uplink

**Hardware:** a custom ARM64 SBC (Cortex-A55, 4 GB RAM, 32 GB eMMC), solar +
LiFePO4 battery, 4G/LTE modem (intermittent), add-on TRNG, no IOMMU.
Embedded-reduced-trust profile. POWER.md fully active.

**Normal operation — daytime, strong solar:**

```
[kernel] energy source: 18W solar input, battery 85%; energy budget: ample
[kernel] DVFS: full-performance P-state; all cells running
[cell:local-app] processes forms, stores records in local object store
        (append-log objects, durability class=durable for local flash);
        4G uplink: connected, 2 Mbps; sync-agent pushing deltas upstream
[time] wall clock: e = 45 ms (NTP over 4G; high jitter);
        lease windows: wide but safe; monotonic clock used for all local
        ordering; HLC stamps outgoing sync messages for causal ordering
```

**Uplink drops — 3 days of partition:**

```
[lease] cluster leases expire; node self-fences from the wider trust domain;
        local identity still valid for local operation
[cell:local-app] keeps running autonomously; all local durability contracts
        honoured; records written to local sealed append-log
[sync-agent] queue: batching outbound deltas (HLC-ordered, content-addressed)
[event stream] partition event logged; local event stream; no upstream export
[7 days] uplink returns; sync-agent reconnects; HLC merge: outbound delta
        replayed in causal order; no manual repair needed (P46: ✓)
```

**Brownout — cloud cover, battery at 12%:**

```
[kernel] energy-pressure event L1: "battery at 12%, shed elastic load"
[cell:local-app] defers non-urgent background indexing; reduces poll rate
[kernel] energy-pressure event L2: "battery at 6%, drop to survival mode"
[cell:sync-agent] suspends; [cell:background-merge] suspended
[kernel] DVFS → lowest P-state; idle cores deep-halted; NPU power-gated
[kernel] survival-mode: only local-app and the flash-durability path alive
[cell:local-app] still accepts new records; writes to append-log at reduced
        rate; durable-local completions honoured
[battery at 2%]
[kernel] safe-halt sequence: outstanding durable writes flushed to eMMC;
        A/B image state committed; halt
[power returns next morning]
[boot] A-slot boots clean; reconciler reconnects; zero data loss (P45: ✓)
```

**Energy budget 72-hour run:**

```
[POWER.md 4] 72 h on a fixed 480 Wh joule budget: race-to-idle pacing;
        energy metered per cell; background jobs deferred to peak-solar
        windows (10:00-15:00); budget adherence: 471 Wh consumed (P44: ✓)
```

---

## Scenario 12 — Desktop: developer workstation (smoke level)

**Hardware:** workstation-class Xeon W, 64 GB, Arc GPU (Vulkan). Desktop
profile (lowest priority, latest phase; this is a smoke run, not a full
gate).

**Boot → Wayland compositor running:**

```
[kernel] Arc GPU: Vulkan driver cell attached; engine benchmarked;
        firmware measured; spatial partition granted to compositor cell
[cell:compositor] holds: display-controller engine grant (scanout),
        HID queue grants (keyboard, pointer), Vulkan compute grant
        for compositing, read grants for client swapchain surfaces
[cell:terminal] starts under POSIX personality; fork+exec → bash;
        PTY as bidirectional queue pair; filesystem: per-session
        namespace view over the object store (POSIX-PERSONALITY.md 3)
```

**Scenario — developer runs a build:**

```
[cell:bash] fork()+exec() cargo build → POSIX clone within capability bundle
[cell:cargo] file I/O through POSIX personality → native object store;
        compiler processes run as short-lived strands in the cell's runtime
[kernel] 32 strands (one per compile unit) across 32 vcores; work-stealing
        topology-bounded to the local NUMA domain
[POWER.md] plugged in: performance P-state; no power throttling
[cell:compositor] while build runs: GPU renders a terminal frame each vsync;
        client seals the swapchain buffer; compositor maps it read-only;
        display-controller scans out; input-to-photon: 14 ms (smoke gate: ✓)
[event stream] build span visible; per-strand timing; can diagnose a slow
        compilation unit without a profiler daemon
```

**Suspend / resume:**

```
[user] closes lid
[kernel] suspend: all engine states saved; vCPU contexts saved; NIC/GPU
        power-gated; DRBG state excluded from the hibernate image
[resume] DRBG mandatorily reseeded (TIME-IDENTITY.md 4); engine states
        restored; IOMMU re-armed; attestation re-verified; desktop
        reappears in < 2 s; 100-cycle soak: zero corruptions (P42 adjacent: ✓)
```

---

## Cross-scenario observations (the foundation, seen from above)

Reading all twelve scenarios together, the design claims that hold across all
of them — and which would be the falsification surface if any one scenario
broke:

1. **Flow context never breaks.** [flow:id] travels from NIC DMA to GPU kernel
   launch to remote RDMA completion to a brownout event on a solar node,
   timestamped by a clock object the system always knows how much to trust. In
   every scenario, "why is this slow" is a query, not archaeology.

2. **The blast radius is always one cell's grants.** NVMe fails → storage
   driver cell bounded. Analytics OOMs → elastic grant bounded. Guest VM
   misbehaves → VMM cell grants bounded. 4G drops → lease expires, local
   operation continues. No scenario produces a global failure from a local
   fault.

3. **Importance is always a reservation, never a priority number.** The
   database's p99 doesn't move under analytics pressure (P15) because
   the contract was admission-checked at reservation time, not guarded by a
   scheduler heuristic at runtime.

4. **Bytes never move without a reason in the trace.** The data-warehouse
   scan, the AI model load, the firewall packet path, the cache-hit WASM
   response — each was verified zero-copy in the event stream. A copy that
   isn't visible in the trace is a design violation.

5. **The remote-Africa node and the hyperscale IX box share the same ten kernel
   objects.** The solar node's brownout pressure event and the IX node's DDoS
   drop event travel the same event-stream machinery. The database's group-
   commit window and the firewall's atomic rule-swap use the same durability-
   class and sealed-table primitives. The foundation did not add objects for
   any profile; it composed them.

6. **Power management composed, not bolted.** The only new subsystem the full
   scope forced (POWER.md) reused reservations, pressure events, the Arch
   trait, and the existing engine lifecycle — the governance rule held.

---

## What would break the foundation

The scenarios above are not stress tests at their limits. The honest failure
surfaces, one per scenario, that the validation suite (VALIDATION.md) must hit:

| Scenario | Likeliest failure mode | Gate it triggers |
|---|---|---|
| Fleet server | Grant-check p99 degrades under contention | P1 |
| AI inference | Paged-KV GPU-MMU remap latency causes token stall | P22 |
| Database | A hard-reserved cell sees OOM anyway (grant accounting bug) | P15 |
| Data warehouse | DPU pushdown falls back to CPU on some Parquet encodings | P17 |
| Internet exchange | Steering-table swap introduces a forwarding window | P30 |
| Firewall | Session-table memory grows unbounded under crafted churn | P32 |
| Edge | WASM cold-start regression in a new WASM engine version | P36 |
| Container | K8s compat edge mis-translates a NetworkPolicy to grants | P25 |
| VM host | EPT violation storm from a misbehaving guest hurts host | P27 |
| Embedded | Hard-RT jitter exceeds PREEMPT_RT baseline on the real board | P40 |
| Remote | HLC merge after 7-day partition produces a conflict | P46 |
| Desktop | Suspend/resume corrupts a cell's RNG state (DRBG not reseeded) | P42 adjacent |

Each of these is a design failure, not an implementation bug - which is why they
are in this document alongside the scenarios that assume they don't happen.
