# Open Questions and Refinements

**Status:** Draft v0.1. Resolves five areas where the core design left
implementation-level decisions open. Each question gets a concrete answer,
not a deferral. Where the answer changes or adds to an existing doc, the
cross-reference is noted.

---

## 1. Graph submission granularity

**Question:** Can applications submit very small graphs (single operation)
efficiently, or is there a minimum batch size? How does the runtime decide
when to batch vs submit immediately?

### The core tension

The graph model must serve two completely opposite workloads:
- A database committing a single 4KB log record needs sub-100µs end-to-end.
  Any graph setup overhead comes directly off that budget.
- A training step spanning 8 GPUs and 3 hosts benefits enormously from
  submitting 10,000 operations in one graph. Overhead per node amortises
  to nothing.

The resolution: **a single-operation "graph" is just a tagged queue entry**.
Graph machinery is invoked only when there are actual dependency edges.

### The tiered submission model

Three submission paths, selected by the runtime automatically:

**Path 1 — Direct submission (zero graph overhead).**
For a single operation with no dependencies: one submission entry on the
relevant engine's queue, doorbell rung. No graph object created, no
descriptor allocated. The flow ID is stamped inline. This path has the same
overhead as a plain io_uring submission - which was always the target.

```
// Runtime sees: one async write, no dependencies, 50µs window
// Takes path 1 directly:
entry.opcode  = WRITE;
entry.handle  = buffer_cap;
entry.flow_id = current_strand_flow();
entry.window  = 50_000;  // nanoseconds
queue.submit(entry);     // doorbell deferred until flush point
```

**Path 2 — Inline graph (small, known at submission time).**
For a handful of operations with explicit dependencies that are all known
before submission: the graph descriptor is allocated from a per-vcore slab
(pre-allocated, lock-free), nodes are written, and the whole graph is
submitted in one doorbell ring. Allocation cost is one slab bump; descriptor
lives in shared memory the kernel can read directly. Suitable for
CPU→GPU→writeback chains up to ~64 nodes.

**Path 3 — Dynamic graph (large, extended incrementally).**
For a training step built up over several milliseconds as tensor shapes are
resolved: the graph object is created upfront, nodes are appended
incrementally, and submission is explicit when the caller calls `seal_and_submit`.
The kernel does not begin scheduling until seal. This is the path for
dependency graphs that span hosts, because cross-host capability references
need to be embedded in the descriptor before submission is valid.

### When the runtime batches vs submits immediately

The doorbell is the batching primitive, and the decision is driven by the
completion window declared on the operation:

```
window <= 50 µs   →  submit immediately (ring doorbell now)
window <= 500 µs  →  hold up to 10 µs for coalescing
window <= 5 ms    →  hold up to 100 µs; batch with neighbours
window > 5 ms     →  batch aggressively; ring doorbell when the
                      hold buffer is full or the deadline nears
```

The runtime's per-vcore submit buffer accumulates entries and rings the
doorbell when any of these trigger: the hold time expires, the buffer is
full (64 entries), or an entry with a tight window arrives and forces a
flush. This is adaptive coalescing - no fixed batch size, driven entirely
by declared latency slack.

One rule that prevents a subtle bug: **a strand never blocks waiting for
its own coalescing window to close**. Submitting an entry parks the strand
immediately; the doorbell is rung by the poller strand on the same vcore
when the coalescing policy fires. The strand does not participate in the
timing decision.

### Minimum viable graph overhead, measured

The design target (to be gate-tested at M0/M1):

| Path | Overhead vs plain memory access |
|---|---|
| Direct (path 1) | ~50 ns (one cache line write + deferred doorbell) |
| Inline graph, 4 nodes (path 2) | ~150 ns (slab alloc + 4 node writes) |
| Dynamic graph, 1000 nodes (path 3) | ~8 µs setup; amortised 8 ns/node |

If path 1 exceeds 150 ns p99, the graph layer is re-examined before the
database profile ships (P13/P14 are the gate).

---

## 2. Error and cancellation

**Question:** How are partial graph failures handled? Is there explicit
cancellation propagation through the dependency chain? Leases seem useful here.

### The completion entry carries status

Every completion entry has a `NodeStatus` field:

```
enum NodeStatus {
    Success,
    Failed { code: ErrorCode, engine: EngineId },
    Cancelled { by: CancellationSource },
    Partial { completed_bytes: u64, error_at: u64 },
}
```

Success delivers the output buffer handle. Every other status delivers a
diagnostic token, not a buffer. The downstream nodes that were waiting for
that buffer receive the diagnostic token as their input.

### Failure propagation — the poison model

When a node fails, its outputs are **poison tokens**. Downstream nodes
whose activation depends on a poisoned input are themselves cancelled with
`CancellationSource::PoisonedInput` — the failure propagates forward through
the dependency chain automatically without any application code running.

Propagation is forward-only: upstream nodes (those the failed node was
waiting on) are unaffected. They were doing work that might benefit other
consumers; they complete normally.

Fan-out is independent: if node A has two outputs going to nodes B and C,
and B fails, C is not affected. Only B's downstream dependents see the
poison.

**Optional vs required inputs**: a node descriptor declares which of its
input edges are required and which are optional (best-effort). A node with
all required inputs present and some optional inputs missing or failed will
still execute, with those optional inputs absent from its view. This lets
a speculative-decoding verify node proceed even if the draft tokens arrived
with a partial-success status.

### Cancellation — three entry points

**Cancel graph:** marks all not-yet-started nodes as cancelled. Nodes
already executing receive a best-effort cancellation signal to their engine
(a control write on the engine's cancellation queue); the engine may or may
not honour it mid-flight. The graph object enters a draining state: it
waits for in-flight nodes to complete (success or failure), then delivers
a single graph-level cancellation completion to the caller's ring.

**Cancel node:** marks one node cancelled and propagates poison forward.
Upstream nodes are unaffected. The caller receives a completion for the
cancelled node immediately, then normal completions for any upstream nodes
that finish.

**Deadline expiry (the lease path):** this is where leases become the
error mechanism. Every cross-host graph edge carries a lease TTL. If the
remote node does not start within its lease window, the lease expires and
the edge delivers a `Failed { code: LeaseExpired }` status — the same
poison mechanism as any other failure. No special case for distributed
failures; leases make them structurally identical to local failures.

### Partial graph failure in practice (the database scenario)

A three-node graph: [write log] → [flush NVMe] → [update index].

- NVMe flush fails (device error): `flush` node delivers `Failed`.
- `update index` receives poison; it is cancelled automatically.
- The graph completes with one `Success` (write log) and two `Failed`/`Cancelled`.
- The application's completion entry shows the first failed node and the
  cancellation reason chain.
- The application retries at the graph level by resubmitting a new graph
  that starts from [flush NVMe] (the write is already durable in the log
  buffer; no need to redo it).

**The application never needs to inspect each node to find out what failed.**
The graph-level completion carries a summary: the set of (node_id, status)
pairs for every non-Success node. One read resolves the error.

### Error isolation between graphs

A failed graph cannot affect another graph's nodes even if they share a
sealed buffer (because sealed means read-only; a failing consumer cannot
corrupt the buffer). The blast radius of a graph failure is that graph's
nodes and downstream dependents — bounded by construction.

---

## 3. Strand → vcore mapping

**Question:** Does the kernel expose a dynamic number of vcores (like
scheduler activations), or are they reserved upfront? How does the runtime
handle strand oversubscription?

### The two-part answer: floor + elastic ceiling

A cell's vcore allocation has two components:

**Floor (hard reservation):** a guaranteed minimum, admission-checked at
cell creation. The kernel will never drop the cell below its floor vcores
regardless of host pressure. The floor is sized for the cell's
worst-case latency requirement — if a cell needs 4 dedicated cores to meet
its RT reservation, those 4 are the floor.

**Elastic ceiling:** additional vcores the cell may use when they are
available, relinquished under host pressure. The ceiling is the maximum the
cell can usefully use (typically: number of runnable strands / 2 is a
reasonable heuristic the runtime computes itself).

The runtime requests elastic vcores by submitting a `REQUEST_VCORES(n)`
entry on its control queue. The kernel grants what it can within the current
pool capacity. Grants and revocations arrive as events on the same control
queue, processed by the runtime's scheduler-activation handler — a specific
strand that runs on behalf of the runtime, not a kernel upcall.

### The activation protocol precisely

**Grant event** (kernel → runtime):

```
VcoreGrantEvent {
    vcore_id: VcoreId,       // which physical core
    duration: VcoreClass,    // Dedicated | TimesharedPool | ElasticBurst
    revocation_notice: Duration,  // how much notice you get before revocation
}
```

On receipt, the runtime's activation strand allocates a per-vcore scheduler
context, picks up the highest-priority runnable strand, and begins executing
it on that vcore. The vcore is now live.

**Revocation notice** (kernel → runtime):

```
VcoreRevocationNotice {
    vcore_id: VcoreId,
    deadline: Instant,       // return it by here or it is taken
}
```

On receipt, the runtime marks that vcore's run queue as draining: the
currently-running strand is preempted at its next yield point (the compiler
yield-point insertion or the doorbell preemption), the strand is parked back
onto another vcore's run queue via the work-steal mechanism, and the vcore
is released by submitting a `RELEASE_VCORE(vcore_id)` entry. If the runtime
misses the deadline (e.g., the strand is in FFI), the kernel takes the vcore
anyway and delivers a `VcoreStolen` event — the runtime's scheduler adjusts
its bookkeeping on receipt.

**Why this works where 1990s scheduler activations failed:**
The original scheduler activations design (Anderson et al., 1991) failed
because the activation handler could be called while the runtime was in a
non-reentrant state (e.g., mid-lock-acquisition), leading to deadlock. Here:

1. The activation events arrive on a queue, not via an upcall. The runtime
   reads them at a chosen point (when the activation strand runs), not at
   an arbitrary interruption.
2. The activation strand is a dedicated strand, not a signal handler. It
   runs in the normal strand scheduler, holding no locks.
3. The blocking-syscall problem does not exist — there are no blocking
   syscalls. Every kernel interaction is a queue submit; a strand never
   disappears into the kernel with a vcore held. The vcore is always
   available for the scheduler.

### Oversubscription handling

Oversubscription (more runnable strands than vcores) is the normal state
for any cell handling bursty workloads. The runtime handles it in three ways:

**Work-stealing between vcores (primary):** each vcore has its own lock-free
run queue (Chase-Lev deque). When a vcore's run queue is empty, it steals
from a neighbouring vcore's tail. Stealing is topology-bounded (prefer
same-NUMA, steal across NUMA only when local is exhausted). The steal
decision is made in ~30 ns without a kernel call.

**Elastic vcore request (secondary):** when the runtime's scheduler detects
that all vcores have been running for more than a configurable threshold
(e.g., 500 µs) without draining to idle, it submits a `REQUEST_VCORES`
event to ask for more. This is rate-limited to avoid oscillation.

**Strand priority within the cell (tertiary):** strands are assigned
priority classes by the application (via the runtime API, not the kernel).
The per-vcore scheduler picks from the highest non-empty priority class.
A latency-sensitive strand (e.g., a database commit) is in a higher class
than a background compaction strand; under oversubscription the commit
always runs first.

**The oversubscription limit:** the runtime should not create more strands
than it can meaningfully schedule. As a guideline: up to 10x oversubscription
(10 runnable strands per vcore) is fine; 100x means strands wait milliseconds
for a turn, which breaks latency contracts. The runtime exposes a gauge
(`runnable_strands / vcore_count`) and the operator sets admission policies
at the cell level accordingly.

---

## 4. Memory model details

**Question:** Are typed memory grants (HBM vs DDR vs CXL) visible in graph
descriptors? How does the runtime request "promote this buffer to HBM"?

### Buffer descriptors carry memory kind

Every buffer reference in a graph node descriptor includes the buffer's
memory kind, because the graph executor needs this to:

1. Validate that the target engine can access the buffer (a CPU-only compute
   node cannot execute against an HBM buffer that is not CPU-accessible).
2. Decide whether to insert an implicit migration node.
3. Emit correct IOMMU mappings for each engine along the path.

```
struct BufferRef {
    capability:       CapabilityId,   // grants the right to use this buffer
    memory_kind:      MemoryKind,     // DDR | HBM | CXL | PMEM | DeviceLocal | Remote
    coherence_domain: DomainId,       // set of engines that see coherent state
    size_bytes:       u64,
    alignment:        u32,
    flags:            BufferFlags,    // Sealed | ReadOnly | Streaming | Pinned
}
```

The `memory_kind` is not a hint — it is the actual physical location,
verifiable by the kernel from the capability's backing grant. A mismatch
between declared kind and actual kind is a capability validation failure.

### Buffer promotion — explicit and implicit

**Explicit promotion (the application knows what it wants):**

A `MigrationNode` is a first-class graph node type:

```
struct MigrationNode {
    source:           BufferRef,       // where the data lives now
    destination_kind: MemoryKind,      // where you want it
    destination:      Option<GrantId>, // None = allocate new; Some = reuse existing
    policy:           MigrationPolicy, // Copy | Move | Alias (where coherent)
    sync:             TimelineSemaphore,
}
```

`Copy` allocates a new buffer of the destination kind and DMAs the data.
The source buffer is untouched and its capability remains valid.

`Move` aliases the data at the new location and invalidates the source
capability after the DMA completes (a revocation on the source grant).
The destination has a new capability.

`Alias` is only valid between engines in the same coherence domain (e.g.,
CPU and an APU's GPU sharing the same physical memory). No data moves;
the destination capability is a read-alias of the source.

**Implicit promotion (the graph executor inserts migration nodes):**

If the application submits a graph where node A produces output in DDR and
node B is a GPU kernel that requires HBM, and no migration node is specified
between them, the graph executor inserts one automatically using `Copy` and
the cheapest available HBM grant. The application can preview which implicit
nodes will be inserted by calling `graph.validate()` before submission — it
returns the full resolved graph including implicit nodes, so there are no
surprises.

**Promotion policy — pinned vs elastic:**

A buffer's grant has a residency policy:

- `Pinned`: the buffer stays in the declared kind until explicitly migrated.
  A pinned HBM buffer is an HBM reservation (counted against the cell's HBM
  grant). The model-weights-in-HBM case from AI-ARCHITECTURE.md is this.
- `Elastic`: the buffer prefers the declared kind but may be migrated by the
  kernel under memory pressure. The cell receives a pressure event before the
  migration happens, giving it a chance to complete any in-flight graph nodes
  that depend on the buffer. After migration the capability is updated to
  reflect the new kind.
- `Streaming`: the buffer exists only for the duration of a single graph
  traversal (common for intermediate tensors). The runtime allocates it from
  a per-graph scratch arena, which is freed in bulk when the graph completes.
  Streaming buffers are never pinned.

**The "promote this buffer" API at the runtime level:**

```rust
// Application code:
let weights: Buffer<Ddr> = object_store.load(model_hash)?;

// Explicit promotion before inference:
let weights_hbm: Buffer<Hbm> = graph.migrate(
    weights,
    MemoryKind::Hbm,
    MigrationPolicy::Copy,
    Pinned,
)?;

// Now a GPU kernel node can reference weights_hbm directly.
// The type system makes it a compile error to pass a Buffer<Ddr>
// to a node that declares Buffer<Hbm> as its input type.
```

The type system catches kind mismatches at compile time for native Lattice
code (the `Buffer<Kind>` generic from the language design). The graph
executor catches them at submission time for dynamically-constructed graphs.
Neither silently falls back.

---

## 5. Debuggability

**Question:** With work flowing through multiple engines and potentially
across machines, how do you provide a coherent "stack trace" or timeline
for a high-level request?

### The answer is the graph itself, made queryable

The graph object is not consumed at submission. It persists in the kernel
(with a capability held by the submitting cell) until explicitly freed or
until all completions have been delivered. During that lifetime the graph is
a live state machine: each node tracks `Pending | Queued | Executing | Done`.

A `GRAPH_INSPECT` verb returns the current state of every node: its status,
which engine it is on, when it started (engine-clock timestamp), how long
it has been executing, and what it is waiting for. This is the multi-engine
equivalent of a stack trace — not a frozen snapshot of a call stack, but a
live view of exactly where in the dependency graph a request currently sits.

```
$ lattice trace --flow a3f2b901
flow: a3f2b901  total elapsed: 48 ms
  [DONE ] node 0  CPU-vcore-3      2 ms    read_request
  [DONE ] node 1  NVMe-0           6 ms    read_log_index
  [EXEC ] node 2  GPU-MIG-0       38 ms    attention_kernel  ← HERE
  [PEND ] node 3  DMA              -        transfer_result   (waiting on node 2)
  [PEND ] node 4  host-7-GPU-1     -        layer_norm        (waiting on node 3)
  [PEND ] node 5  CPU-vcore-1      -        encode_response   (waiting on node 4)
elapsed breakdown: read 8ms / gpu-exec 38ms / transfer tbd / remote tbd
```

### The coherent timeline from flow IDs

Every event in the system — NIC completions, DMA timestamps, GPU kernel
start/stop, strand wakeups, lease events — carries the flow ID from its
triggering graph node. These are collected by the exporter cell and written
to the OTel backend.

The timeline is reconstructed from these events using HLC timestamps:

1. Events from the same engine are trivially ordered (monotonic clock).
2. Events from different engines on the same host are comparable because
   engine clocks are calibrated to a shared monotonic reference at attach
   time (TIME-IDENTITY.md 1) with a known, bounded offset.
3. Events from different hosts are HLC-ordered: the causal chain is preserved
   even when wall clocks differ, because every cross-host message carries an
   HLC that is strictly after all events the sender has seen.

The result is a **causally-correct timeline** across CPUs, GPUs, DMA
engines, NICs, and remote hosts — with each event placed in time with an
error bound e, not a single imprecise timestamp.

### The swim-lane view

The OTel exporter constructs a distributed trace with one span per graph
node, parent-child relationships matching the dependency edges, and timing
from the engine-clock timestamps. Standard tracing UIs (Tempo, Jaeger,
Honeycomb) render this as a Gantt chart with swim lanes per engine:

```
Time →                    0        10ms      20ms      30ms      40ms      50ms
CPU-vcore-3    ╠══read═╣
NVMe-0                 ╠═════read_index═════╣
GPU-MIG-0 (local)                           ╠══════════attention═══════════╣
DMA                                                                         ╠══╣
host-7/GPU-1                                                                    ╠═══
```

Gaps between spans are scheduling latency (a strand was runnable but no
vcore was available), transfer time (the data was in flight), or queueing
time (the engine was busy with another node). The breakdown is automatic —
the timestamps make it visible without manual instrumentation.

### Strand-level "stack trace"

For the CPU side of a request, the runtime's introspection capability
(OBSERVABILITY.md 7) exposes the full strand state at any moment:

```
$ lattice strand-trace --cell inference-server
strand 4821  EXEC   attention_dispatch → graph:a3f2 node:2   47ms
strand 9103  PARK   waiting: graph:a3f2 node:2 completion
strand 9104  PARK   waiting: graph:b801 node:0 completion
strand 2     EXEC   poller (vcore 0)
strand 3     EXEC   poller (vcore 1)
```

The "waiting on graph node X" state is the distributed stack trace. It shows
exactly what chain of events must complete before strand 9103 can run —
including if that chain goes to a GPU on a remote host.

### What "p99 is slow" looks like to investigate

A practical debugging workflow:

1. Alert fires: inference p99 > 80 ms for the last 5 minutes.
2. `lattice query --flow-stats --window 5m --p99` returns the flow IDs of
   the 1% slowest requests in that window.
3. `lattice trace --flow <slowest-flow-id>` shows the swim-lane breakdown:
   "GPU attention kernel: 62 ms. Normal is 38 ms."
4. `lattice engine --inspect GPU-MIG-0 --window 5m` shows: "reservation
   miss rate 12%. Adjacent MIG partition ran an unbounded kernel for 28 ms
   at T-41ms."
5. Root cause: a batch job was co-located on an adjacent MIG slice with an
   insufficient preemption contract. The placement engine is given a new
   constraint: this inference cell and that batch job may not share a
   physical GPU.
6. The fix is a desired-state update — one `kubectl annotate` equivalent
   — that the placement engine reconciles in seconds.

Total time from alert to root cause: under 5 minutes, using only the
system's own event stream. No log scraping, no per-service instrumentation,
no distributed tracing agent to configure.

### The non-negotiable: debuggability is not optional

Each of these tools — graph inspect, swim-lane timeline, strand trace — must
exist before a profile ships. A profile that cannot be debugged in production
is not production-grade (PRODUCTION.md 1). The WASM probe system
(OBSERVABILITY.md 3) handles cases where the built-in instrumentation is
insufficient: attach a probe to a specific graph node type and capture
additional detail without recompiling or redeploying the workload.

---

## Summary — the five refinements as design decisions

| Question | Decision |
|---|---|
| Graph granularity | Three submission paths; path 1 is a tagged queue entry with zero graph overhead; batching is doorbell-driven by completion-window slack, not a fixed batch size |
| Error and cancellation | Poison-token forward propagation; graph-level cancel + node-level cancel + lease-expiry as a uniform failure source; graph-level completion summary with one read |
| Strand/vcore mapping | Floor (hard) + elastic ceiling; activation events on the control queue (not upcalls); activation strand handles grants/revocations at a safe scheduling point; works because there are no blocking syscalls |
| Memory model in graphs | `BufferRef` carries `memory_kind` as a verified field; `MigrationNode` is a first-class graph node; explicit or implicit (graph executor inserts); type system catches kind mismatches at compile time for native code |
| Debuggability | Graph object is live and queryable; HLC-ordered engine-clock timeline reconstructed from flow IDs; strand introspection shows "waiting on graph node X" as the distributed stack trace; swim-lane view standard via OTel export |

---

## 6. Admission control backpressure (gap surfaced by pressure testing)

**Problem:** The current design treats reservation admission as binary: accepted
or rejected. Under bursty, unpredictable load this produces brittle
"all-or-nothing" behavior. Two concrete symptoms: (1) an inference server
requesting more HBM for KV cache growth gets a hard rejection at the worst
moment - peak load - instead of a signal to shed earlier; (2) a long-running
compaction job getting its vcores revoked mid-graph has no way to ask "can I
have 200ms more to finish this node cleanly?"

**The gap:** no mechanism between "hard reservation" (consume now, guaranteed)
and "no" (rejected). The missing primitive is a **tentative reservation**: a
commitment to deliver a resource at a future point, or a structured alternative
if that commitment cannot be made.

### Design: tentative reservations

A new verb alongside the existing reserve/release pair:

```
// Submit a tentative reservation request:
TentativeRequest {
    resource:          ResourceClass,      // HBM, vcores, NVMe-bandwidth, ...
    amount:            u64,
    needed_by:         Instant,            // deadline: when you actually consume
    priority:          ReservationPriority,
    alternatives_ok:   bool,               // accept a different resource kind
}

// Kernel response (delivered as a completion event):
TentativeResult {
    status: enum {
        Granted,                           // token ready; call Claim(token) to consume
        Deferred { available_at: Instant },// can't commit until then; re-request
        Partial { amount: u64 },           // only this much is committable
        Suggest { kind: ResourceClass,     // alternatives_ok=true: here is what I have
                  amount: u64 },
        Denied,                            // will not be available by needed_by
    },
    token: Option<ReservationToken>,       // non-null on Granted/Partial/Suggest
}
```

A `Granted` result holds the resource earmarked (not yet allocated) until
`needed_by`. At that point the cell calls `Claim(token)` — an atomic,
always-succeeds operation because the resource was pre-committed. A
`Claim` on an expired token fails cleanly (the earmark lapsed), which is
the only failure mode and is easy to handle.

### How this closes the two symptoms

**KV cache growth under load:**

```
// Instead of: grant = request_hbm(2 GB)  // may reject at worst moment
// The inference server does:
tentative = request_tentative(HBM, 2 GB, needed_by=now+80ms, alternatives_ok=true)

match tentative.status {
    Granted  => { /* token ready; claim when the next request arrives */ }
    Partial  => { /* evict LRU prefix-cache blocks to fit within partial amount */ }
    Suggest  => { /* CXL is available; accept 2.3x latency for this batch */ }
    Deferred => { /* current batch will free HBM in 60ms; hold new requests */ }
    Denied   => { /* begin graceful shedding: reject new long-context requests */ }
}
```

The inference server gets a rich signal to act on rather than a surprise
rejection at the moment it most needs the resource. Each status maps to a
concrete policy decision with a known cost.

**Vcore revocation during compaction:**

The revocation notice (from OPEN-QUESTIONS.md section 3) already gives the
cell a deadline. Adding tentative reservations lets the cell ask whether an
extension is feasible:

```
// On receiving VcoreRevocationNotice { deadline: T }:
if current_graph_node_completes_by(T) {
    // yield normally at T
} else {
    ext = request_tentative(VCORE, 1, needed_by=T, needed_until=T+200ms)
    match ext.status {
        Granted  => { /* continue; yield at T+200ms */ }
        _        => { /* checkpoint the graph node now; yield at T */ }
    }
}
```

The compaction knows whether to checkpoint or continue based on a
pre-commitment from the scheduler, not a race between the graph node
completion and the kernel's deadline.

### Admission control returns guidance, not just verdicts

The broader principle: the admission control system is a **scheduler
cooperative** — it has information the runtime needs (when resources will
free, what alternatives exist, how congested different resource classes are)
and the runtime has information the scheduler needs (when it will actually
use the resource, how urgent it is, what it can shed). Tentative reservations
are the protocol through which these two exchange that information.

This does not change the hard guarantees: a claimed reservation is as
guaranteed as a hard reservation. It adds a coordination layer between
"I might need this" and "I definitely need this now."
