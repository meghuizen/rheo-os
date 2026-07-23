# Time, Ordering, Identity, and Randomness

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.5; the clock/entropy object
(section 3, object 9). Boot-time entropy in BOOT.md 4; RNG security in
SECURITY-IDENTITY.md.

Position: three problems OSes habitually blur, kept strictly apart - **what
time is it** (clock sync), **what order did things happen** (causality), and
**unpredictable bits** (entropy). Each has one owner and one honest contract.
Uncertainty is made explicit rather than hidden behind a single perfect clock.

## 1. Time - intervals, not instants

The kernel never reports "the time is T." It reports "the time is in
[T-e, T+e]" - a bounded interval (Google TrueTime / AWS ClockBound style).
Hardware reality promoted to the API.

- Three clock types are distinct kernel objects:
  - **Monotonic** - never jumps; for durations, timeouts, leases. The default.
  - **Wall** - an *estimate* with error bound e; for humans and cross-system
    timestamps.
  - **Engine clocks** - GPU/NIC timestamp counters mapped with known
    offset/drift, so a NIC-timestamped completion is comparable to CPU time
    (essential for tracing DMA chains, OBSERVABILITY.md).
- **Sync:** PTP-first (NIC hardware timestamping, nearly free given the
  DPU/SmartNIC design), NTS as fallback, ideally a GNSS or atomic reference
  per fabric island. The sync daemon is an ordinary cell holding a capability
  to *discipline* the clock - nothing else can step it.
- **Authenticated time is mandatory** (NTS, PTP with MACsec). Unauthenticated
  time is an attack vector against everything that consumes it: lease expiry,
  certificate validity, capability TTLs.
- **e is queryable.** A cell asks "current bound?" and gets ~50 us on a PTP
  island or ~5 ms on NTP fallback. Leases consume it directly: a lease is
  valid until T minus the joint error bound, so a host with degrading sync
  sees its effective leases shrink and **self-fences** (SECURITY-IDENTITY.md
  3). Clock quality is a safety input, not a dashboard curiosity.

**The rule that follows:** wall-clock timestamps never decide ordering or
uniqueness. They are for humans, logs, and external interop only.

## 2. Ordering - hybrid logical clocks

Causal order is what distributed systems actually need, and physical time
cannot give it. Every cross-host message - queue transfers, state-store
writes, capability grants - carries an **HLC** timestamp (physical component +
logical counter). HLCs stay close to real time but never violate causality.
The state store uses them for versioning and watch ordering
(CONTAINERS-KUBERNETES.md 3). Cheap (64-128 bits), stamped by the transport
layer, invisible to applications unless they want it.

## 3. Identifiers - UUIDv7 as the convention

Every kernel object needs a cluster-unique ID; v7 (48-bit ms timestamp +
random) is the default because IDs are created everywhere without coordination
and land in ordered indexes - the time prefix means B-tree appends instead of
random-insert thrash, which matters enormously for the state store and the
object store's namespace indexes (FILESYSTEMS.md 3). Three caveats built in:

- The timestamp inside a v7 ID is **hint, not truth** - never parsed for
  logic, only exploited for index locality. Ordering truth lives in HLCs.
- v7 IDs leak creation time. IDs are typed (like memory); for tenant-visible
  objects where that is sensitive, a **v4-random** kind is offered. Explicit
  per object class, not a global default.
- Within one host, node-scoped **64-bit handles** (engine/queue handles) are
  cheaper and get promoted to full UUIDs only when they cross the host
  boundary.

## 4. Randomness - per-cell DRBGs

Replaces Linux's global-pool / blocking-lore / random-vs-urandom confusion:

- One **root DRBG** per host, seeded from hardware sources (RDSEED/RNDR, TPM,
  NIC/board TRNGs, jitter entropy as a floor), continuously reseeded, health-
  tested (SP 800-90B). Post-boot on modern hardware, "running out of entropy"
  is a myth; seeding is the only critical moment.
- Every cell gets its **own DRBG instance**, seeded from the root at creation,
  reseedable via a capability. Getting random bytes is a **library call in the
  cell**, not a syscall - fast path costs nothing, no cross-cell side channel
  through a shared pool, and a compromised cell's RNG state reveals nothing
  about siblings'.
- **Fork/clone/restore safety is structural:** checkpoint/restore *must*
  reseed the cell DRBG on every restore, kernel-enforced, because resumed-VM /
  cloned-snapshot RNG reuse is a real bug class (duplicated ECDSA nonces,
  repeated TLS keys). The DRBG state is deliberately excluded from the
  checkpoint image (ARCHITECTURE.md verb set; VIRTUALIZATION.md 8).
- **Boot-time entropy is attested:** the measured-boot chain includes the
  entropy source configuration, so a host proves *what* seeded its root DRBG.
  A diskless node with no TRNG and no sealed seed fails attestation rather
  than silently minting weak keys (the Mining-your-Ps-and-Qs failure class).
  See BOOT.md 4.
- VMs get virtio-rng feeding plus their own jitter + mandatory reseed - trust
  but supplement (VIRTUALIZATION.md, EMULATION.md 3).

## 5. How it hangs together

Authenticated time bounds make leases and capability TTLs safe; HLCs make the
state store and audit logs causally ordered; UUIDv7 makes indexes fast without
pretending IDs are clocks; per-cell attested DRBGs make every key and nonce in
the identity system trustworthy. Time, order, identity, and entropy each have
one owner and one contract.

## 6. Open question

Whether the state store needs TrueTime-style **external consistency** (commit-
wait on e, Spanner-style, buying strict serializability at the cost of
coupling write latency to clock quality) or **HLC-based causal+** consistency
is enough. For an infrastructure store - low write rate, high read rate - the
current default is HLC without commit-wait, but it is an explicit open
decision (ARCHITECTURE.md 9).
