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

## 11. What is built

Everything above is the design. This section is the ledger: what exists in code,
proven on all three ISAs, and what is still only written down. Phase names are the
ones in the implementation plan.

### 11.1 The five planes

Observability is not one structure, because it is not one question. Each plane gets
the cheapest shape that answers its own:

| Plane | Question | Where | State |
|---|---|---|---|
| Text | what did it say | `kernel/src/telemetry.rs` | built (per-CPU, folding, host-fuzzed) |
| Event | what happened, in order | `kernel/src/trace.rs` | built but **single-CPU**, 6 windows |
| Distribution | how long did it take | `kernel/src/metrics.rs` | built; **6 of 8 metrics have no recorder** and no boot enables it |
| Counter | how many | ~30 hand-written accessors | scattered, unindexed |
| Snapshot | what is it doing **now** | - | not built; this is the htop data source |

The spine that indexes them is `kernel/src/obs/`, and `abi/src/obs.rs` is the
layout all three readers agree on.

### 11.2 S0 - the ABI and the root (done, all three ISAs)

`abi/src/obs.rs` defines the plane's layout once, for three separately-compiled
readers: the kernel that writes it, an in-guest collector cell, and a **host tool**
that reads guest physical memory with no cooperation from the guest. That third
reader is why the layout is in `rheo-abi` (zero-dep, `no_std`, no lang items) and
not in the kernel.

`kernel/src/obs/root.rs` exports one page-aligned symbol, `RHEO_OBS_ROOT`, whose
section table carries both a kernel VA and a **physical address** per region, plus
the tick domain, tick rate, CPU counts and the live window mask.

**The load-bearing claim is that an outside reader can walk it from the ELF alone**,
and it is verified rather than argued. The reader's algorithm is: resolve the
symbol's VMA, find the `PT_LOAD` containing it, and compute
`pa = p_paddr + (vma - p_vaddr)`. Hand-computing that from `readelf` output and
comparing against what the guest published gives the same address on every ISA:

| ISA | symbol VMA | segment vaddr/paddr | hand-computed PA | guest published |
|---|---|---|---|---|
| x86-64 | `0xffffffff8047f000` | `0xffffffff80400000` / `0x400000` | `0x47f000` | `0x47f000` |
| ARM64 | `0xffff0000404a7000` | `0xffff000040400000` / `0x40400000` | `0x404a7000` | `0x404a7000` |
| RISC-V | `0xffffffc08068d000` | `0xffffffc080600000` / `0x80600000` | `0x8068d000` | `0x8068d000` |

Two independent computations of the same fact agreeing is the proof; the
`observe` test kernel asserts the guest half, and nothing in the path is
ISA-specific or QEMU-specific.

**Timestamps are raw ticks, and this is the design's first real constraint.**
`arch::timer_now_ns()` cannot be on an emit path: it is a 128-bit multiply and
divide on all three ISAs, where riscv64 has no 128-bit divide instruction (so the
divide is a call into `__udivti3`, a software loop) and aarch64 additionally
re-reads `cntfrq_el0` and executes an `isb` every call. A tracer built on it would
cost more than the code it observes, which is the one thing a tracer must not do.
So `arch::obs_tick()` is one counter read with no barrier - `rdtsc` / `mrs
cntvct_el0` / `rdtime` - and `tick_hz` is published for conversion at the edge.
Dropping the barrier loses no ordering: within a CPU order comes from the event's
sequence number, and across CPUs from merging on the tick.

Measured resolution, which is why `tick_hz` is published at all rather than assumed:
**1 ns/tick on x86-64, 16 ns on ARM64, 100 ns on riscv64** (QEMU `virt`'s 10 MHz
timebase). Intervals below a machine's own tick are not resolvable there, and a
reader is expected to decline to print such a number rather than invent one.

Layout decisions and their reasons: the event record is **32 bytes**, so with a
page-aligned frame it never straddles a 64-byte cache line (at 40 bytes it straddles
~40% of the time and an emit dirties two lines instead of one); there is **no drop
counter**, because loss is a property of a reader's cursor - `head - capacity - c`
events are missing and the range `[c, head-capacity)` says exactly which, located
rather than counted, and one fewer atomic read-modify-write on the emit path; and
`abi_hash` is a compile-time hash of every structure size rather than a build id,
because the kernel has no build-id mechanism and a constant sitting in such a field
would be a field that lies.

Three negative controls, two firing and **one recorded as a non-result**:

- The const initialiser stops writing the magic -> the identity assertion fires
  (`left: 0, right: 5929065116637020755`). This is the check that makes a wrong
  address report "no root" instead of decoding garbage.
- `region()` publishes the virtual address where a physical one belongs - the
  forgotten linear-map mask, and the single most likely mistake, because it makes a
  host reader silently decode nothing -> `section kind 6 publishes pa
  0xffffffff80476c70, which is not anywhere physical memory is`. The oracle is
  deliberately not a recomputation of `virt_to_phys` (which would only show the
  publisher agrees with itself) but a question about the machine: does this land
  where memory is?
- **`#[used]` removed -> the symbol survives on all three ISAs.** It is not
  load-bearing today, because `publish()` takes the static's address and
  `boot::init` calls it on every kernel, so there is a real reference and nothing
  for a garbage collector to take. It is kept because that reference is incidental
  to the plane's purpose, not because a control fired - stated so the attribute is
  not mistaken for a proven guard.

One real finding came out of publishing a field rather than reasoning about it:
`smp::online_count()` answers "CPUs that SMP bring-up **registered**", and the only
thing that registers the boot CPU is `smp::init`, which exists only under the `smp`
feature - so it returns **0 on every single-CPU boot**, while a CPU is demonstrably
executing the call. Correct for the question `smp` asks it, and a plain falsehood in
the field a reader uses to size every per-CPU view, so `obs::root` floors it at the
CPU doing the publishing.

Also landed here, as prerequisites rather than as their own claims: `repr(C)` on
`telemetry::Ring`/`Rings` and `metrics::Histogram`, and `repr(transparent)` on
`smp::PerCpu<T>`, so that a reader outside the guest can stride those arrays with a
guaranteed layout rather than one that happens to hold.

### 11.3 Not built yet

Per-CPU event rings (closing `trace.rs`'s own single-CPU admission), the window
mask and the eight new windows, the snapshot plane and per-CPU busy/idle
accounting, the counter unification, lock instrumentation, syscall tracing, the
capability gate on telemetry, egress beyond the serial console, the host tool, and
the OTLP exporter cell. Dynamic probes remain the documented deferral of section 5.
