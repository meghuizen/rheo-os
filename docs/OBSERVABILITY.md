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
| Event | what happened, in order | `kernel/src/obs/ring.rs` | built, **per-CPU** (S1); `trace.rs` is now a shim |
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

### 11.3 S1 - the per-CPU event ring (done, all three ISAs)

`kernel/src/trace.rs` said what was wrong with it: the ring was "one shared buffer
with a plain counter, so it is single-CPU today", and the fix was "deliberately not
copied here until a multi-core boot wants to trace". This is that fix.
`kernel/src/obs/ring.rs` is one ring per CPU, each with its own sequence counter,
safe by **partitioning** rather than by hoping - the argument `telemetry` already
made. `trace.rs` becomes a ~180-line shim: `Subsys`, `Kind`, `emit`, `enable`,
`counters`, `dump` all keep their signatures and the `@E` format is unchanged,
because `cargo xtask trace` parses it and `tests/src/smp.rs` asserts on it. Renaming
a module is not a proof, so nothing was renamed.

**Two design decisions were forced by writing the code**, and both are corrections to
the plan rather than details of it.

*Funding cannot happen on the emit path.* The plan said a CPU would fund its ring
lazily on first emit. Funding allocates, allocation takes `mm::frames`' pool lock,
and one of the recorded windows traces the allocator - so an emit that funded on
demand could re-enter the frame allocator from inside it, on a lock that is not
recursive. That is a deadlock, not a slow path, and it would appear only on the first
event a boot ever recorded from that window. `fund_this_cpu()` is therefore a
bring-up act and `emit` never allocates; a CPU with no ring counts its offered emits
instead of losing them. One thing fell out for free: `fund` publishes `capacity`
**last**, which was written so a reader could never see a funded ring pointing at
nothing, and it also makes funding self-excluding - the allocator's own events see
`capacity == 0` and land in the unfunded counter rather than in the ring being built.

*The `@E` header changed and the host tool's gap detection had to.* A sequence number
is now per-CPU monotone, and `cargo xtask trace` compared consecutive lines of the
merged stream - which would report a gap at every point where the emitting core
changed. Pure noise on any multi-core boot, and the tool's own rule is that a
diagnostic which cries wolf is worse than none, so gaps are detected per CPU. Events
are dumped in per-CPU blocks rather than merged on the tick: a k-way merge costs a
scan of every cursor per line for an ordering the host can produce by sorting, and it
would put a ~2.5 KiB working set on a kernel stack at the moment the machine is being
inspected. Timestamps are converted to nanoseconds **here**, at the edge, relative to
the plane's origin tick - which is the point of recording raw ticks at all.

`trace::counters()` keeps returning `(written, lost)`, but the second number is
**derived** now: a ring holds `RING_EVENTS`, so anything past that has been
overwritten and no increment on the emit path is needed to know it. Same meaning as
before - "a total computed from this dump would be incomplete", which is exactly what
`smp`'s trace phase asserts - reached without an atomic read-modify-write per event.

**Proof.** The `observe` kernel drives the plane end to end on all three ISAs: the
ring funds from the real pool (**17 frames** - 16 data plus a directory, as designed),
300 records come back **field-for-field** with real ticks advancing, the ring wraps
and the surviving window is exactly `[total-cap, total)` with the record before it
gone and the one after it not yet written, and reset returns all 17 frames.

The multi-core claim - the whole reason this replaced a shared buffer - needs two
cores, so it lives in `smp`. Two cores record 64 events each **at the same instant**
(through `smp::run_fn_with_secondary`, which rendezvouses first so the overlap is
real), and three things are asserted per ring rather than as a total: each ring took
exactly its own core's 64; every record is found in the ring of the core that wrote
it, identified by a tag in its own contents rather than by a count; and each stream's
sequence numbers are consecutive, which is the property the host tool's loss detection
rests on and the one a shared counter destroys. Observed: `cpu0` and `cpu2`, 128
total, 17 offered to an unfunded ring (the secondary's own funding allocations,
exactly), 34 frames returned.

Controls, three firing and **one recorded as a non-result**:

- Every CPU forced onto ring 0 - `trace.rs`'s own shape -> `the primary's ring took
  145 of its 64 events`, which is 64 + 64 + 17 in one ring with the other empty. The
  defect stated as arithmetic. Note what it is *not*: under TCG nothing was lost, so a
  count of total events would have looked fine. What breaks is attribution.
- The slot mask written `n & cap` instead of `n & (cap - 1)` -> the host fuzzer fails
  four wrap cases and the release case by name.
- `seq_of` made zero-based -> "a zeroed frame reads as a written event". Sequence
  numbers are one-based because a funded frame arrives zeroed, so a `seq` of 0 must
  mean "never written" rather than "written first".
- **`ObsRing::get`'s sequence-number check does not fire**, measured rather than
  assumed: removing it leaves every fuzzer case passing. Sequentially the bounds test
  subsumes it, because nothing can recycle a slot between a bounds check and a read
  when nothing else runs. It earns its keep only against a reader racing a live writer
  on another core - which no built reader is yet - and reproducing that would mean
  aliasing the ring mutably to model a data race the language forbids. Kept as what
  makes the later collector and host tool sound, documented in `ring.rs` as reasoned
  rather than proven.

`verify/obs/fuzz.rs` is the host driver, included in `cargo xtask verify`. It checks
the wrap against an independent `VecDeque` oracle at four starting points, including
one that crosses **2^32** - which matters because the recorded sequence number is
`head`'s low 32 bits, and four billion events is about 71 minutes at one event per
microsecond. `u64` wrap is deliberately not tested: at one event per nanosecond it is
584 years, so the ring does not claim to survive it and asserting that it does would
be inventing a requirement.

### 11.4 S2 - the window mask, and what "off costs nothing" actually costs

A single on/off flag is the wrong control. The useful request is almost never
"narrate everything": a boot chasing a frame leak wants `Kmeta` and `Frames`, and
turning `Syscall` on beside them buries the six lines that matter under thousands.
So the mask is per window, selection happens at the **source** where an event costs
nothing to not produce, and the mask lives **in the published root** rather than in a
private static - one copy, so a reader sees exactly what the kernel consults and no
mirror can disagree with it.

Five new windows get their first call sites, each at a seam that is already the single
place its subsystem funnels through:

| Window | Where | What the fields carry |
|---|---|---|
| `Syscall` | `linux::handle` | enter/exit pair; number and first argument, then the outcome |
| `Irq` | `net_rx::on_irq` | arrival count and which line |
| `Timer` | `ktimer::register` / `cancel` | acquire/release per **client**, so a lost deadline is visible |
| `Queue` | `queue::kernel_process` | opcode packed with status; the flow id |
| `Mem` | `linux::mem` fill and COW | acquire for a fill, **transfer** for a copy-on-write private |

Each is one place for the same reason `linux::plock` is auditable from one line.
`Timer`'s acquire/release-per-client shape is chosen because this module exists at all
due to two subsystems arming one hardware one-shot and destroying each other's
deadlines (docs/NETSTACK.md 16 N2h) - and a lost deadline is invisible in a total.
`Net`, `Gpu` and `Lock` are deliberately left for the slices that bring their counters
and their instrumentation; a window with no call site would be a name pretending to be
a feature.

Two honest narrowings are recorded where they happen rather than done quietly. The
queue window carries the **low 64 bits** of the flow id: a flow id is 16 bytes because
it is W3C-traceparent shaped (section 2), and carrying it whole would consume both
payload fields of a 32-byte record and so cost the opcode and status - within one boot
the low half identifies a flow, and an exporter that needs all of it has the submission
entry, where it is not truncated. And the syscall exit record collapses the outcome to
one number, distinguishing a returned value from "this cell is about to block".

**The cost claim is now a measurement, and getting there took two defects of my own.**

The plan listed "move the mask test after argument marshalling" as a *control to run*.
Writing the first version as an ordinary function call committed it instead: Rust
evaluates arguments before the call, so `obs::emit(W, K, owner, expensive(), ...)` pays
for `expensive()` with the window off, and `cargo xtask bench` showed **+9 instructions
per queue round trip** while nothing was recorded. `obs_event!` is a macro so the mask
test sits outside the argument expressions.

That recovered one instruction, not nine - so the cost was not the marshalling. The
remainder was the compiler placing the cold call's register setup *before* the branch
meant to skip it. `#[cold]` on `emit_now` moves the path out of line, and the delta
fell to **+3**: exactly the load, the `and` and the not-taken branch the design claims.
Neither of these would have been found by reading the source.

**And then the enabled path, where three attempts made it worse before deleting code
made it better.** The first measurement was 60 instructions per event. The reasoning
said the cost was `Funded`'s page directory - a bounds check, two more branches and a
**dependent load** before the store - and that consecutive events hit the same frame
128 times running, so caching the frame would remove almost all of it. Caching it
changed **nothing** (60 -> 60): the directory entry is in a hot line, and a load from
a hot line is not what a 60-instruction path is made of.

Reading the **disassembly** answered in one look what two rounds of reasoning had not:
**15 of the ~46 instructions were prologue and epilogue** - six callee-saved register
pushes and pops - and they existed because the `#[cold]` refresh function behind the
cache was a *call*, which forces LLVM to keep live values in registers it must save.
An optimisation for a path taken one time in 128 was costing 15 instructions on the
other 127. Inlining it recovered 6; deleting the cache entirely and indexing the
directory directly recovered 14 more, and left `push` with a single `push %rbx`.

**A second pass took the enabled path from 46 to 40, by doing what ftrace does.** The
question that prompted it was the right one - Linux under PREEMPT_RT cannot afford a
half-baked trace path, so what does it do that this did not? Point by point:

| | Linux (tracepoints + ftrace ring buffer) | this plane |
|---|---|---|
| disabled site | a **patched NOP** (static keys rewrite kernel text) | load + test + branch, **3** |
| record write | **in place, as words** - reserve/commit, no struct copy | was: build `ObsEvent`, 7 stores |
| metadata | packed header word, constants folded at compile time | was: 4 sub-word fields, 3 register moves per site |
| nesting | NMI-safe via a local cmpxchg, paid on every event | single producer per CPU, **no nesting cost** |
| timestamp | delta-encoded TSC read | one raw counter read |

Two of those rows were real gaps and both closed. `pack_meta` is a `const fn` packing
owner/window/kind into the one u64 they occupy in the record's last eight bytes, so
the constant half **folds into a single immediate at the call site** (a site with a
constant owner passes the whole word as one `movabs`, replacing three register moves)
- the `TRACE_EVENT` trick of assembling the header word at compile time. And
`push_packed` writes the record **in place as four u64 stores** - tick, `a`, `b`,
packed-word-or-sequence - where the first version built an `ObsEvent` and let the
compiler copy it field by field, seven stores, four of them sub-word. On ARM64 the
four pair into **two `stp` store-pair instructions**. The layout equivalence between
the four words and the `ObsEvent` fields is a compile-time assertion on every field
offset, plus a little-endian assertion so a big-endian port fails at the assumption
rather than by decoding swapped fields.

**The last of it took an architecture change, taken because this is greenfield.** The
ring stored its events in `Funded` pages and paid a page split - a shift, a mask and a
dependent directory load - per recorded event, purely because `mm::frames` had no
contiguous allocation path. That absence was a choice made for good reasons (`Funded`
exists so nothing needs contiguity), and it had already cost once before: the GICv3
ITS work fell back to statics, recording "the frame allocator offers neither
contiguity nor that alignment". With two real callers the admission test is met, so
the pool grew **`frames::alloc_contig(n)`** - first-fit under the existing lock,
zeroing exactly as `alloc` zeroes, freeing per-frame through the ordinary `free`, and
refusing honestly under fragmentation rather than panicking. The ring is now **one
contiguous 64 KiB block**: the emit path is `base + slot * 32` with nothing between
the slot arithmetic and the stores, the published header carries one `base_pa` instead
of a directory to walk (four ABI fields deleted), a host reader's job is a **linear
read**, and `obs/ring.rs` now depends on *nothing* - the block, the tick and the
physical address all arrive from the caller, so the host fuzzer's `Funded` shim was
deleted outright and more of the shipped code runs under test. A funded ring is 16
frames now where 11.3 reported 17: the directory frame is gone.

Read from the disassembly afterwards, not assumed: the function is **22 instructions
with no prologue at all** on x86-64, and every one is irreducible - the four stores,
the head load and store, the counter read and its 3-instruction combine, the
capacity guard, the slot arithmetic, one register save, `ret`.

| | x86-64 | riscv64 | aarch64 (scaled) |
|---|---|---|---|
| `obs_emit_off` | 5 | 5 | ~4 |
| `obs_emit_on` | **37** (was 60) | **40** (was 53) | **~33** (was ~52) |

(`obs_emit_off` includes the benchmark's own two `black_box` loads, so the mask test is
the 3 instructions above; `obs_emit_on` likewise carries ~9 of bench harness - the
function itself is 22. aarch64's counter advances at 62 ticks per 1000 instructions, so
its raw numbers are scaled by that calibration.) For scale, in the same run an
interrupt entropy mix is **15** instructions and one `rng_next_u64` draw is **367**.

The alignment contract earned its keep immediately: `verify` builds with debug
assertions on, and the first host run of the contiguous ring **fired the new
`read_volatile` alignment check** - the fuzzer had handed it a `Vec<u64>` block, merely
8-aligned, where `ObsEvent`'s align(32) (the never-straddles-a-cache-line property)
demands 32. In the kernel the contract holds for free because frames are page-aligned,
so no boot test could ever have caught a caller that broke it; `fund` now asserts it,
and the fuzzer allocates the type it claims to hold.

Where Linux still wins, named rather than matched: **the disabled site**. Static keys
patch the branch to a NOP in the kernel text, so an off tracepoint costs ~0 against our
3. Matching it means self-modifying kernel text - a write window over RX pages, and
per-ISA I-cache/pipeline synchronisation (x86 needs the breakpoint-dance protocol,
ARM64 cache maintenance plus ISB) - which is a large, security-sensitive mechanism to
buy back three instructions that are already measured as noise. Refused for now, with
this paragraph as the record of what it would take. Where Linux pays *more*, also
named: its ring-buffer reserve/commit is nesting-safe against NMIs via a local
cmpxchg on every event, because anything can trace inside anything there. This plane's
producer is partitioned per CPU and the kernel takes interrupts only in its idle
paths, so no emit can interrupt an emit today - the cost is not paid, and the
constraint is stated in `obs/ring.rs` so the first design that lets a handler
interrupt kernel-context code knows to revisit it.

The append path is now the one place in the tree that writes through a `Funded`
directory without `Funded`'s bounds check, and the cost of that is stated rather than
elided: an off-by-one there is a wild store instead of a refusal. It is paid for with
two `debug_assert!`s, which are free in a kernel build and **live** in
`verify/obs/fuzz.rs` - `cargo xtask verify` now builds with `-C debug-assertions=on`,
because a model checker is exactly where a cheap invariant check should fire. With the
slot mask broken the fuzzer prints `slot 2048 past capacity`; before the flag it
segfaulted.

**On removing the flow id, which was the other candidate**: it is not where the cost
is. `entry.flow_id as u64` is one narrowed load on the queue path only, and the 46
instructions above are measured with plain constants - no flow id involved. Dropping
the field would buy about one instruction and give up following a request across the
queue, the graph and the wire, which is section 2's primitive and the reason the design
carries flow context at all. Kept.

Per-benchmark, against the pre-S2 baseline: every path with a new call site on it costs
**+3** (`p2_roundtrip_single` 243011 -> 246011 milliticks/op, `p2_roundtrip_batched64`
167417 -> 170417, `p2_user_roundtrip` 333007 -> 336007), and every path without one is
**unchanged to the tick** (`p1_grant_check`, `p2_ring_push_pop`, `p2_doorbell_trap`,
`p3_context_switch`, `p5_crosscell_roundtrip`, `entropy_mix_event`). The two new
benchmarks are permanent, so the claim stays measured rather than becoming folklore.

Proven by `observe`'s mask phase on all three ISAs: one of three windows enabled records
**exactly** 20 of 60 offered events (an exact count, because a mask leaking one window
into another would still record fewer than everything), the records that landed are
asserted to be the enabled window's rather than merely the right number, a second window
takes effect with no re-funding, and clearing the mask records nothing at all - off
meaning off, not buffered for later.

The performance pass did not stop at the emit: the same "where is the cost actually
paid" question, asked of `mm::kmeta::Funded` - the table under the entity, VMA and
vcore state the event windows record about - produced the inline directory, the
by-reference scan API and the fused decision-path walks. That work and its measured
numbers live with the mechanism in docs/SUBSTRATE.md pillar 1 (S2c), and the lessons it
forced - optimise the access shape not the access, two questions about one element set
are one walk, bench the whole matrix because a struct's size is an interface - are
recorded in docs/ENGINEERING.md 11.

### 11.5 The snapshot plane and busy/idle time (S3)

**Built: the plane that answers "what is this CPU doing now."** One
`abi::obs::ObsCpu` per CPU (512 bytes = 8 cache lines, so a reader sampling CPU 3
never touches CPU 4's lines): line 0 is a **seqlock'd coupled group** - state,
current cell/entity/vcore, when that began, the armed timer deadline, the receive
tier - and lines 1..7 are monotone counters outside the lock, because each counter
is independently meaningful while the *group* torn in half is a cell that never ran
an entity that never existed. Written only by the owning CPU at transitions it
already passes through (`user::enter_vcore`, the end of `run_inner`, `idle::wait`'s
park bracket, `ktimer`'s re-arm, `net_rx`'s tier escalation) - **no sampler, no
timer, no IPI**. Published as `OBS_SEC_CPU`, with a name table (`OBS_SEC_NAMES`)
saying which counter slot means what, so a reader takes the meaning from the kernel
it is actually reading.

**Busy/idle is real time, measured at the one place each transition happens.**
`since_tick` is the stamp; every transition charges `now - since` to busy or - only
when the park genuinely halted - to idle. A park that could not halt charges
**busy** and counts a spin, because a spin is not idle and recording it as one
would launder exactly the number this plane exists to make honest. The armed
deadline is published in the arbiter's own **ns domain** and the field says so
(`timer_deadline_ns`) - converting per re-arm would put a multiply on the pacer's
continuous re-arm path to make the field prettier.

**The writers are behind their own mask bit** (`W_SNAPSHOT`, a modifier like
`W_LOCK_HOLD`): the disabled cost on the context-switch and idle paths is one load
and a not-taken branch, and the bench matrix against the pre-S3 build shows **every
path unchanged to the tick** (the only deltas are the `rng_*` static-layout ripples
from S2c moving back to their pre-S2c values - which is itself the confirmation
they were layout, not code).

**The seqlock's orderings are proven where they are reachable.** Begin is
`fetch_add(1, Acquire)` (a store cannot carry Acquire; the RMW keeps field writes
below the odd count), end is `store(+2, Release)`; the reader re-checks behind an
Acquire fence. QEMU TCG cannot interleave a 6-store window and the in-kernel reader
runs on the writer's own CPU, so `verify/obs` drives the shipped `cpu.rs` verbatim
on real host threads: **4.8M coherent reads racing 3M writes, zero torn groups**,
with busy+idle exact to the write count afterwards - and the negative control (the
same stores, bracket deleted) **is caught torn within ~25k reads**, so the
invariant detects what it claims to. One oracle correction recorded: the
arithmetic check's first version said "half each" for 4999 odd and 5000 even
intervals, and the counters refuted it - the failing assertion was the hand
computation, not the code.

Proven in-QEMU by `observe`'s snapshot phase on all three ISAs: every gated
counter zero while the writers are off (a nonzero would mean a writer escaped its
gate; the halt/spin counts left this list when S4 made them unconditional - 11.6), a
**20 ms park charges ~20 ms of idle ticks** (19.93M of 20M expected on x86-64,
judged through the root's own published `tick_hz` - the bound proves attribution,
not timer precision), a real U-mode cell entry writes the group and counts a
dispatch, and after the run the group says kernel context - a group still claiming
the cell would be a live-state lie.

**The machine-wide memory block** (`abi::obs::ObsMem`, `OBS_SEC_MEM`) closes S3:
DDR and pmem pool numbers, NUMA fallbacks, the demand-paging witness pair
(`recorded_pages`/`eager_pages`), block-cache fills and the kernel's own kmeta
charge, **filled on request** (`obs::mem_refresh`) rather than maintained -
every field is already live in its own subsystem, and stamping a mirror from
`frames::alloc` would put a store on the hottest allocation path. So the block
carries `refreshed_tick`: a reader judges staleness instead of being lied to
about it, and `observe` asserts exactly that - the unrefreshed block says tick 0,
a refresh matches `frames::stats()` exactly, three allocated frames appear as
exactly three after the next refresh **and not before** (a block that moved
without a refresh would mean an allocator is keeping the mirror warm, the cost
the design refuses). Per-node used/total breakdowns wait on per-node counters in
`frames` itself, which is S4-adjacent work, named rather than approximated.

### 11.6 S4 - counter unification (begun: `net_rx`, `input`, `idle`)

**One counter, wherever a count had two homes or none safe.** The counter plane is
`ObsCpu`'s 56 slots plus three helpers - `obs::cpu_bump` on this CPU's own block
(returning the running count, so an interrupt handler that needs a sequence number
pays no second read), `cpu_counter_sum` over every CPU, and `cpu_counter_clear` for
the reset paths - and the migration rule is the plan row's own: **every accessor
keeps its signature**, so the existing suite's assertions on those accessors are the
migration's exactness oracle. Bumps are **unconditional**, not behind any mask bit,
because the kernels asserting these counts never enable recording - a counter that
only counts while observability is on is a different quantity wearing the same name.
The cost is what the statics cost (one volatile read-add-write on the owning core's
own line, no lock), and where the old counter was one global `static mut` bumped
from whichever core ran the path - racy the moment a second core did - the per-CPU
slots make the SMP case correct and cheaper at once.

Migrated: `net_rx`'s five (NIC irqs - whose running count doubles as the entropy
sequence number - spin polls, timer slices, halts, escalations), `input`'s three
(console bytes / entropy sequence, pump FIFO takes, pump direct pushes), and
`idle`'s two, which were the **double-report** the plan named: `idle` kept its own
unconditional statics while `obs::snap_unparked` bumped `CTR_HALTS`/`CTR_SPINS`
behind the mask, so the same halt counted once or twice depending on a bit. One
counter now - `idle::wait` bumps the plane slots unconditionally and the snapshot
writer keeps only the gated *time attribution* - and `observe`'s zero-while-off
assertion **moved deliberately rather than being weakened**: halts left the zero
list, and a new bracket parks with snapshots off and asserts the count moves while
`CTR_IDLE_TICKS` stays zero, the split contract (count unconditional, time gated)
asserted as itself.

Three controls, two firing and one honest non-result. Misrouting the NIC-irq bump
into the halts slot fails `netwait` by name on aarch64 ("interrupt-driven ISA but
the kernel never took a NIC interrupt"); gating the idle bumps behind the mask - the
wrong unification - fails `observe`'s new bracket by name. The first control
attempted, misrouting the hot-tier spin-poll bump, **did not fire**: `netwait` has a
stall-tolerance branch that reads 0 spins as "the guest was stalled past the
busy-poll budget" and skips exactly the assertions that would have caught it.
Recorded rather than shrugged off - a control a tolerance branch can absorb proves
nothing, which is why the control that stands is on a counter whose assertion has no
tolerance.

Reset topology was checked before the move, not after: `obs::reset` (which zeroes
whole blocks) and the module `reset()`s are called by **disjoint kernels**, so the
plane clearing between runs changes no kernel's observations, and each module reset
clears exactly its own slots through `cpu_counter_clear` under the same between-runs
contract every reset in the tree already states. `input`'s byte sequence is
deliberately not cleared, matching the old `RX_SEQ`: it is an entropy sequence
number, and monotonicity across runs costs nothing.

The scheduler's counters followed in the second slice: `sched::dispatch`'s six
(picks, round-robin picks, diverged, charged ns, and the E5 re-arm pair) and
`sched::preempt`'s six (armed, taken, unarmable, to-sibling, to-cell, notes) - the
hottest counter sites in the tree, one bump per pick and per slice, where the
relaxed `fetch_add`s these replaced were a locked RMW on a line shared by every
dispatching core and the per-CPU slot is a plain read-add-write on the owner's
own. Gated by `preempt`/`substrate`/`smp`/`linuxproc` on all three ISAs, with the
control observed firing by name: misrouting the taken-count into the armed slot
fails `preempt` with "61 slices armed but none was ever taken".

Still to migrate (the rest of the plan row): the arbiter's arms/firings/parks/
preserved (already per-CPU inside the `Arbiter` - publication candidates more than
relocation candidates), smp's claim counters (already-correct feature-gated
atomics with exact multi-core assertions - plane-readability is the only gain),
and the frames/nvme/block/load/mm/rng::entropy families - each a mechanical
repetition of the proven pattern, gated per module. Deliberately **not**
migrating: gauges (`kmeta::META_FRAMES`, the pool's `USED` - the plane's slots are
monotone counters, and a gauge in a counter slot invites rate arithmetic over a
non-rate), config statics (profiles, VAs, search hints), per-cell
Linux-personality state (per *cell* rather than per CPU, behind `plock`), and
`sched/entity.rs`'s `double_entries`/`affinity_skips` - that file is
**host-included verbatim** by `verify/entity`, so a `crate::obs` call inside it
would break the fuzzer's shim, the same structural rule that put S3's `snap_user`
in `user.rs` rather than beside the entity table. Those two stay SMP-sound atomics
where they are; the plane's query surface (S7/S9) reads accessors, not slots, so
nothing is lost but relocation.

### 11.7 S5 (lock half) - named locks, contention, wait and hold time

**Built: a `SpinLock` that can say who it is, and instrumentation that measures
contention and only contention.** `SpinLock::named(value, LockId)` opts a lock in
(`LockId` a closed kernel-defined set, like `TimerClient`: the frames pool, the
pmem pool, the console, the admission ledger, the entropy pool, the NVMe queues,
and a `Probe` id so a proof can contend on a lock that is not load-bearing);
`SpinLock::new` is untouched and id 0 is never measured. The fast path is now one
**strong** `compare_exchange` - strong deliberately, because a weak CAS may fail
spuriously on LL/SC ISAs and every failure enters the contended path, whose count
must mean "the lock was genuinely held" for the uncontended-equals-zero oracle to
hold. The `#[cold]` contended path counts the contention and the spin iterations
per lock (`obs/lock.rs` - per-**lock** atomics rather than per-CPU plane slots,
because "which lock is hot" is the question and per-CPU slots would answer "which
CPU waited"), records `Metric::LockWaitNs`, and emits a Lock-window event naming
the lock. Hold time is behind its **own** modifier bit (`W_LOCK_HOLD`): measuring
it reads the clock inside the critical section and lengthens the very region it
measures, so a wait-only run gives contention with no perturbation of the held
region - and the guard records the hold **after** the release store, so the
measured region never includes the recording.

**The recording rule the frames lock forced**: lock metrics go through
`metrics::record_noalloc`, never the allocating `record` - the pool lock is itself
named, so a first-sample bucket allocation from inside its guard is a
self-deadlock, and even after release the allocation re-enters `metrics` through
the *inner* guard's drop while the outer `&mut` histogram is live, which is
aliasing. Bucket storage comes from `metrics::prefund`, a bring-up act - the S1
fund-never-on-emit rule, third application. `Metric` gained `LockWaitNs` and
`LockHoldNs` (METRICS 8 -> 10); metrics enabling is per-CPU, which shaped the
proof - the secondary enables and prefunds for itself, and assertions sum over
CPUs.

**Contention is proven deterministically, not statistically**: TCG's interleaving
is far coarser than a critical section, so two cores hammering symmetrically can
serialise entirely and a `contention > 0` assertion on that shape is flaky by
construction. The `smp` phase instead has the primary take the probe lock, hold it
until the secondary *declares* its acquire, and keep holding through a generous
delay - so the secondary's CAS lands on a genuinely held lock every time. Observed
on all three ISAs: **16 of 16 handshakes contended** (2.7-3.3M spins, 16 wait
samples, p50 from a real prefunded histogram), **1024 uncontended acquisitions of
the same named lock = exactly 0 contentions** (the plan's own control, asserted
in-line: nonzero would mean the counter counts acquisitions), and the hold split -
0 hold samples with the modifier off across all of the above, >= 1 after one
acquire with it on.

**The disabled cost is measured and published**: the whole 102-benchmark matrix
against the pre-S5 build moved on exactly one line - `entropy_absorb_32B`, the one
op that takes a named lock per iteration, at **+11 milli-instructions/op** on
x86-64/riscv64 (+0.5 aarch64) = the id test, the mask load, the guard's zeroed
hold field and the drop's compare, ~0.8% of that op. Every other benchmark,
including every path through the *unnamed* nineteen locks, is unchanged to the
tick. The plan estimated ~4 instructions; 11 is the honest number.

### 11.8 S5 (device half) - the NIC, storage and GPU panes

**Built: three status panes under `ObsMem`'s discipline** - `ObsNet`,
`ObsStorage`, `ObsGpu` (`abi/src/obs.rs`, 128 bytes each, published as
`OBS_SEC_NET`/`STORAGE`/`GPU`), filled **on request** (`obs::net_refresh` etc.)
and stamped with when, never maintained on a driver's hot path. A pane is a
*view*: the counts it copies are live in the drivers and the counter plane
already. What had to be added at the source: the NIC driver's frame/byte counts
each way (plane slots, bumped at the one point a frame is genuinely sent or
taken - beside the entropy hook, for the same reason) and the virtio-gpu
present/byte counts; the storage pane reads the NVMe driver's existing honesty
counters (per-core submits, cross-core = 0, poll fallbacks, reordering) and the
block cache's fills. The NIC pane also carries the receive wait's escalation
counters, its idle mode, and `interrupt_driven` - so the pane says whether the
machine parks or polls instead of leaving it inferred. **GPU utilisation is
reported absent, not estimated** (`util_plus_one = 0`): no device QEMU models
exposes a busy signal and there is no vendor driver to ask, and `librheogpu`
asserts the absence so an estimate cannot creep in.

Proven where the round trips already are: `librheonet` asserts the unrefreshed
pane says tick 0 (a pane that moved un-refreshed means a driver keeps a mirror
warm - the cost this design refuses), then after refresh exactly 1 TX frame /
42 B (its ARP request) and >= 1 RX frame (SLIRP's padded reply); `librheogpu`
asserts presents >= 1 with `present_bytes` **exact** (presents x 128x128x4).
Two controls observed firing by name: the TX-frame bump removed fails
`librheonet` with "counted 0 frame(s) / 42 byte(s)" (the bytes still counting is
the surgical signature), and the fix below re-armed the once-absorbed control.

**A recorded non-result became a fix**: 11.6's absorbed control - `netwait`'s
stall-tolerance branch reading a misrouted spin counter as a stalled guest - is
closed by publishing the kernel's own answer: `net_rx::last_spin_budget()`, the
effective hot-tier budget of the most recent wait. A genuine stall means the
wait *computed* a zero budget (the hot window had expired before it ran); a
nonzero budget with zero counted spins means the counter is broken, and the
skip branch now asserts exactly that distinction. The once-absorbed misroute
now fails by name ("the spin counter is broken, not the guest stalled") -
observed.

### 11.9 S6 (first slice) - the distribution plane records for real

**Five of the six unrecorded histograms gained their recorder at their honest
owner**: `QueueNs` per entry at the one bridge point every submission passes,
`SyscallNs` at the **Linux personality dispatch** (the surface Node/Bun/Claude
Code exercise - the native verbs are measured by their own planes, and
bracketing them too would double-charge the same work), `FaultNs` on successful
demand-page fills only (a fault that becomes a signal is not a fill service
time - `bracket_cancel` exists for exactly that path), `BlockNs` at the block
cache's device-read seam whichever driver is underneath, and `SwitchNs` at the
**scheduler's** switch sites in `nproc` - deliberately *not* inside
`switch_native_cell_vcore` itself, below. **`NetRttNs` is deferred with its
reason written on the metric**: an RTT is the transport's own measurement and
the transport is userspace by doctrine; a kernel-side wait duration wearing that
name would be false.

**The disabled cost was unacceptable on the first attempt, and the fix is the
S2b lesson applied to the distribution plane.** Written as
`let t0 = if enabled() { now_ns() } else { 0 }`, the stamp stays **live across
the measured region** - the whole opcode dispatch, the whole switch - and the
register pressure lands with recording off: measured +12..+42 instructions per
queue/switch round trip. The `metrics::bracket_start`/`bracket_end` form stores
the stamp in a per-CPU slot (one per metric, so different metrics nest - a fill
inside a syscall - while a metric's own bracket never does at its sites) behind
an `#[inline(always)]` gate with a `#[cold]` body: the disabled path is a load,
a test and a not-taken branch with nothing live after it. Re-measured: **+2..6
per queue round trip (~3 per gate, the S2 bound), everything else unchanged**.

**And the switch bracket moved rather than being paid for**: even the bracket
form's two gates cost a measured **+34 instructions per `SYS_SWITCH` round trip
on riscv64** inside the small, hottest-primitive switch function (isolated by
removing exactly those two gates: p5 761008 -> 759008). The raw verb's constant
cost is already a headline `bench` number; the *variance* the metric exists to
expose lives at the scheduler's call sites, so the bracket lives there
(`nproc::reschedule` and the yield path), and `p5_crosscell_roundtrip` is at or
below its pre-S6 value on all three ISAs.

**A third aliasing hazard closed by ordering**: the allocating `record` no
longer holds `&mut` into the histogram set across the first-sample bucket
allocation - taking the frame drops the pool lock's guard, and with
`W_LOCK_HOLD` on that drop re-enters metrics through `record_noalloc`.

Proven on all three ISAs where the paths already are: `librhearun` (exactly 8
`QueueNs` samples for its 8 echoes), `schedidle` (4 scheduler switches),
`linuxpoll` (116-129 syscalls + 205-247 fault fills, means plausible per ISA),
`blockfs` (exactly 16 device reads). Control observed firing by name: the
`FaultNs` record removed fails `linuxpoll` with "the recorder is dead".

Still ahead in S6: entity `ready_ts` for a correct `RunDelayNs` (with the plan's
revert control), and the fairness / balance / starvation / reservation exports.

### 11.10 Not built yet

The per-node memory breakdown, the rest of the counter unification (11.6), the
rest of S6 (11.9), the capability gate on telemetry, egress beyond the serial
console, the host tool, and the OTLP exporter cell. Dynamic probes remain the
documented deferral of section 5.
