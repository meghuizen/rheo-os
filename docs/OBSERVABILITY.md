# Observability - Tracing, Events, and Probes

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.10; the event-stream object
(section 3, object 10). Developer-facing tooling in TOOLING.md 6.

Position: observability is foundational, not a layer - but split correctly.
**OpenTelemetry is the export format, not the kernel mechanism.** The kernel
provides three native primitives: always-on flow context that rides every
queue entry, a typed event stream built from the same queue machinery as
everything else, and capability-gated dynamic probes (the DTrace/eBPF role).
OTel semantics map onto these at the edge.

## 1. Why foundation-level

In Linux a request's journey disappears at every boundary (syscall, io_uring,
DMA, another process, the NIC), so tracing means heroic context propagation in
every runtime, and the ecosystem built eBPF + uprobes + sidecars to
reconstruct what the kernel threw away. This design is *made of* explicit
boundaries - every operation is a submission entry, every dependency a graph
edge, every cross-cell hop a typed message. The causality OTel tries to
recover already exists as data structures; discarding and re-deriving it would
be the one genuinely stupid move available.

## 2. Primitive 1 - flow context in the ABI

- Every Tier-1 entry, Tier-2 message, and transport frame carries a 16-byte
  flow ID + flags, deliberately W3C-traceparent-shaped so edge mapping is a
  copy, not a translation.
- The runtime stamps it from strand-local context; the **kernel propagates**
  it through dependency graphs, DMA nodes, and remote queues without
  interpreting it. Cost: 16-24 bytes per entry, memcpy propagation, nothing
  when disabled.
- The payoff no Linux system has: **the trace follows the DMA.** "Load
  checkpoint shard -> NVMe read -> NIC RDMA -> HBM write -> GPU kernel launch"
  is one flow ID across four engines and two hosts, timestamped by engine
  clocks the time design already made comparable (TIME-IDENTITY.md 1). GPU
  kernels and storage ops appear in the same trace as the RPC that caused
  them.

## 3. Flow-ID lifecycle (span open/close, fan-out/fan-in)

- A **span** opens when a flow ID is created or forked at an operation
  boundary and closes on the matching completion. Because operations are
  explicit queue submit/complete pairs, span open/close are not manual
  instrumentation calls - they are the submit and completion events
  themselves.
- **Fan-out** (a dependency graph node with multiple children, a broadcast
  stream, a scatter): each child edge derives a child flow ID from the parent,
  producing a **span tree** that mirrors the graph exactly.
- **Fan-in** (join nodes, collectives, gather): children close into the parent
  span; the join's completion is the parent's close. A distributed training
  step's span tree therefore *is* its dependency-graph shape
  (AI-ARCHITECTURE.md 6), with RDMA transfer and collective nodes as spans.

## 4. Primitive 2 - the event stream is just another queue

- Kernel and runtimes emit typed events (IDL-defined schemas, Tier-2 encoding,
  same evolution rules - DATA-FORMATS.md 3) into per-cell / per-vcore
  lock-free ring buffers. Scheduling decisions, grant checks, pressure events,
  lease expiries, strand parks - all events, all carrying flow IDs.
- A collector is an **ordinary cell holding a read capability** to event
  streams. This quietly solves the observability-security problem Linux never
  solved: tracing visibility is capability-scoped, so a tenant traces exactly
  its own cells, an operator with a broader grant sees the host, and "root
  sees all keystrokes via eBPF" has no equivalent.
- Ring discipline: bounded, **drop-with-counter** on overflow (observability
  never backpressures the workload), and drops are themselves visible events.

## 5. Primitive 3 - dynamic probes (DTrace/eBPF role)

- **Static tracepoints** (kernel + runtime, always compiled in, ~zero cost
  dormant) plus **dynamic instrumentation** as a debug capability.
- Probe programs are **verified WASM components** (the same machinery as the
  network dataplane, NETWORKING.md 4): type-checked against the event schemas
  they attach to, resource-bounded, terminating. That is the eBPF role with a
  sounder verification story - a probe cannot crash, block, or slow the target
  beyond its declared budget, and *attaching* one needs a minted grant with an
  audit trail, versus eBPF's root-or-nothing and verifier-escape CVEs.
- Strand-aware by construction: probes see strand IDs, wait-for edges, and
  flow context, so "why is p99 slow" drills cluster trace -> cell -> vcore ->
  strand -> the mutex it parked on, in one tooling chain.

## 6. OpenTelemetry at the edge

- An **exporter cell** subscribes to event streams and flow-ID lifecycles,
  maps them onto OTel spans/metrics/logs (semantic conventions included), and
  speaks OTLP to any backend - Grafana/Tempo/Jaeger work day one.
- **Tail-based sampling** gets uniquely good here: complete flow data exists to
  sample *from*. Sampling is exporter policy; the kernel always propagates,
  rarely stores.
- **Metrics largely come free:** the capability metering (CONTAINERS-
  KUBERNETES.md 6) *is* the metrics source - CPU budgets, grant consumption,
  queue depths, reservation misses - exported as OTel gauges with no
  instrumentation in the workload.

## 7. Runtime introspection (making 100k strands observable)

Because the kernel sees vcores, not the strands (CONCURRENCY.md 9), each
runtime **must** export a standard introspection capability: strand dumps,
wait-for graphs, per-strand accounting. Without it, 100k strands are
unobservable, so it is mandatory, not optional. This is the same interface the
dynamic probes attach through.

## 8. Three refusals

- **No printf-logging as a system citizen.** Logs are events with schemas;
  human-readable rendering is an exporter concern. A freeform debug-string
  event type exists for bring-up, shamed but present.
- **No unbounded always-on capture.** Flow IDs always; payloads and
  high-cardinality events only under active grants. Observability data is
  tenant data - same capability, retention, and tenancy rules as everything
  else, not a shadow database of everyone's behavior.
- **No debugger back door.** Interactive debugging (stop a strand, read
  memory, single-step) is a policy-minted, audited debug grant on a specific
  cell - versus Linux's global ptrace-scope sysctl fight (TOOLING.md 6).

## 9. The compounding payoff

Because scheduler decisions are events and capability denials are events, the
classic unanswerable questions become queries over one causally-ordered,
HLC-timestamped corpus: "why didn't my workload get placed," "who was denied
access to what," "where did my latency budget go across three hosts and a
GPU" - instead of archaeology across dmesg, journald, auditd, kubelet logs,
and a service mesh's access logs.

## 10. Honest costs

- 16-24 bytes per queue entry is measurable single-digit-% overhead on the
  smallest RPCs; the flow header is exactly one cache line with the entry
  header, not a generous struct.
- Schema-typed events raise the friction of ad-hoc "just print here" during
  bring-up (mitigated by the freeform debug event type).
- WASM probes will not match hand-tuned eBPF JIT for per-event nanoseconds in
  year one.
- The exporter cell is security-critical - it aggregates cross-tenant
  visibility by design, so its grants and output path need state-store-grade
  scrutiny.
