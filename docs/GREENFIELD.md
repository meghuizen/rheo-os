# Designing an OS today: the lineage, what we take, and what we refuse

The question this document answers: **if you start afresh, knowing everything the last
fifty years of systems research produced, what do you build?** Including the ideas that
were good and never landed - because "not mainstream" is often a statement about
compatibility pressure and vendor economics, not about whether the idea was right.

## The method, and why it comes first

Every entry below is judged by three tests, in order, and most candidates fail one:

1. **Does it beat what this tree already has?** Not "is it interesting". A great idea we
   already implement differently is a note, not a task.
2. **Does it fit, or does it fight?** An idea that needs an ambient-authority syscall, a
   dynamic in-kernel code loader, or a shared mutable kernel is not an addition here - it
   is a different operating system wearing our name.
3. **Can it be *proven* here, and if not, is the fallback the tested path?** The tree's own
   law (docs/ENGINEERING.md 1): a capability is claimed only from evidence the code cannot
   fake. An idea whose only demonstration is a hardware lab is designed for and gated, not
   claimed.

The third test is why this document names, for each item, what would count as evidence.
A research idea adopted without a gate is a decoration.

**Nothing in this document is built.** It is the reasoning behind the design, plus a list
of open work with the evidence each would need. Where an item is already implemented, it
says so and cites where.

---

## 1. The lineage already in the design

Stated compactly, because the interesting part is section 2. These are ideas from prior
work that this tree already implements, so they are not proposals:

| Idea | From | Where here |
|---|---|---|
| Capabilities, no ambient authority | KeyKOS -> EROS -> Coyotos, seL4 | the whole object model (docs/ARCHITECTURE.md 3) |
| Per-core kernel state, explicit cross-core messages | **Barrelfish** (multikernel) | docs/SCHEDULING.md 1a, `smp.rs`, per-CPU everything |
| Completion rings as the *native* I/O ABI | io_uring, but retrofitted there | `abi/`'s queue ABI - native, not bolted onto fds (docs/IO.md) |
| Userspace drivers and network stack | SPDK/DPDK, Snap, Arrakis | docs/DRIVERS.md, `net/` as portable userspace |
| Personality layers, not one ABI | Mach, NT | `Personality::{Native,Linux}` - Linux is a *guest* of the design |
| Zero-copy sharing by handle | dmabuf, shmif | sealed grants delegated read-only, epoch-revocable |
| Tracing as a primitive, not a bolt-on | DTrace, OTel | flow context in the ABI (docs/OBSERVABILITY.md 2) |
| Budgets, not overcommit-and-kill | Nemesis, seL4 MCS | no OOM killer; per-cell frame budgets (docs/MEMORY.md 7) |
| EEVDF + burst-aware fairness | CFS -> EEVDF, BORE | `sched/{vcore,bore}.rs` |
| Model-checking the state machines | Hyperkernel, CertiKOS | `verify/`, `cargo xtask verify` |

The pattern worth naming: **almost every one of those was research first and took ten to
thirty years to reach production, and several still have not.** That is the prior for
section 2 - the filter on new ideas should be "is it right", not "has it shipped".

---

## 2. Good ideas that have not landed, and what to do with each

### 2.1 Contract-checked channels and sealed processes - Singularity / Midori

**The idea.** Singularity (Microsoft Research) gave every channel a **contract**: a state
machine, written down, saying which messages are legal in which order. The compiler
checked that both ends obeyed it. Processes were **sealed** - no dynamic code loading
after start - which is what made whole-program analysis possible. Midori carried it into
an async-everywhere language runtime with a two-tier error model: *recoverable* errors as
values, and *abandonment* for broken invariants, with a supervisor above.

**Why it never landed.** It required a managed language end to end and rejected C
compatibility, which was commercially fatal in 2008 and is much less so now that Rust
exists.

**Does it beat what we have?** Yes, and it fills a directory that is currently a stub.
`idl/` exists in this tree with nothing in it, and the queue ABI's opcodes are checked at
run time (`STATUS_DENIED`) rather than at compile time. A protocol state machine that is
*checked* rather than *documented* is the difference between "the cell got a refusal" and
"the code could not be written".

**Does it fit?** Unusually well. Cells are already sealed by construction - W^X is
constitutional, and the one exception (a JIT) is a capability minted by a launcher, which
is exactly Singularity's "sealed unless explicitly unsealed". And `Rights<MASK>` +
`SubsetOf` (docs/KERNEL-RUST.md 2) already puts *capability rights* in the type system, so
putting *protocol order* there is the same technique one level up.

**Gate.** An IDL that generates both ends of one existing protocol - `net::service`'s
`Request { op, client, seq }` is the obvious first - such that a client sending an
out-of-order message is a **compile error**, and the generated code is byte-compatible with
the current wire format so every existing proof stays valid unchanged.

**What to refuse from it:** the managed-runtime requirement, and static verification of
*everything*. The contract is worth having on channel protocols; extending it to whole-cell
analysis is a research project, not a slice.

### 2.2 Microsecond-scale core reallocation - Shinjuku / Shenango / Caladan

**The idea.** Latency-critical and batch work do not need to *share* a core by
time-slicing; they need cores **reallocated between them at microsecond scale**. Caladan's
contribution is the sharpest: interference (on memory bandwidth, on LLC, on
hyperthread siblings) is *detectable* at that timescale, so a central allocator can move
cores in response to measured interference rather than to a static policy.

**Why it never landed.** It needs a dataplane OS, dedicated cores, and a resource
allocator nobody wants in a general-purpose kernel. Linux has no place to put it.

**Does it beat what we have?** Partly, and it names the next question rather than a defect.
This tree already reallocates *work* by claim (a core takes the next runnable vcore) and
already has the measurement discipline (`metrics.rs`, per-CPU histograms, jitter defined
once as P95-P50). What it does not have is **the control loop**: nothing observes
interference and moves work in response. The EEVDF+BORE queue decides order within a core;
Caladan's question is how many cores a workload should have this instant.

**Does it fit?** Yes, and it is the natural home for the P/E/LP core classes
(docs/EXECUTION-MODEL.md 2.1) - "which class, how many, right now" is one decision.

**Gate.** Two cells, one latency-sensitive and one throughput-bound, on a machine with
more cells than cores, where the latency cell's P99 is asserted to stay inside a bound
while the batch cell's throughput is *reported*. **This is honestly lab-gated**: QEMU TCG
time-slices vCPUs onto host threads and models no cache or memory-bandwidth interference,
so the *mechanism* can be proven here and the *policy* cannot. Saying which is which is
the whole discipline.

### 2.3 Scheduling policy as a userspace program - ghOSt

**The idea.** Google's ghOSt moves scheduling *policy* into userspace agents while the
kernel keeps only the mechanism, so a policy can be changed, A/B-tested and rolled back
without a kernel change.

**Does it beat what we have?** It sharpens something already half-present. This tree has a
two-level scheduler (kernel schedules entities, `runtime/` schedules strands) and exports
the burst score read-only so both levels use one metric. ghOSt's extra step is that the
*kernel-level* policy is also replaceable. The honest assessment: that is worth having
eventually and is **not** worth doing before E2-E4, because a replaceable policy over three
disagreeing representations of an entity would multiply the defect class
docs/EXECUTION-MODEL.md 1 describes rather than remove it. Sequencing matters more than the
feature.

### 2.4 Pointers that outlive the process - Twizzler, and the single-level store

**The idea.** Multics and IBM i had it and the world forgot: **one namespace for memory and
storage, no serialisation boundary.** Twizzler (UCSC, recent) revives it correctly for
NVM: references inside a persistent object are **cross-object references**, not virtual
addresses, so an object can be mapped anywhere by anyone and its internal pointers still
resolve. No pointer-swizzling, no serialise-on-write.

**Why it never landed.** It needs byte-addressable persistent memory to be worth it, and
NVDIMMs arrived and then largely left. CXL makes the question live again.

**Does it beat what we have?** Yes, for the workload this tree explicitly targets. PMEM
grants exist (`MemKind::Pmem`, real on x86-64 via the ACPI NFIT), and the analytical/
warehouse direction is stated. But a `Grant` today is *bytes*; a columnar index or a B-tree
in it must be rebuilt or swizzled on map. Cross-object references would let a dataset carry
live structure.

**Does it fit?** The capability model helps rather than hinders - a cross-object reference
is naturally "an object id plus an offset", and an object id is a capability. That is a
better fit than it is in a POSIX system, where the equivalent is a file path.

**Gate.** A persistent index built by one cell, unmapped, and traversed by a *different*
cell at a *different* base address with no fixup pass - asserted bit-identical to a
freshly-built one. The `librheodata` columnar dataset is the workload already in the tree
to do it with.

### 2.5 Charge the work to whoever caused it - Nemesis

**The idea.** Nemesis (Cambridge) observed that in a conventional OS, work done *on behalf
of* an application - driver interrupt handling, network stack processing, page-cache
eviction - is charged to the kernel or to whichever process happens to be running. So QoS
guarantees are unenforceable: an application can consume another's budget through the
driver. Nemesis pushed that work into the application and accounted for all of it.

**Why it never landed.** It required restructuring every driver, and the multimedia QoS
market it targeted did not materialise.

**Does it beat what we have?** It names a **real gap that is about to open**. Today the
kernel does driver work in trap context and charges nobody. That is tolerable while
drivers are in-kernel and there is one client. The moment docs/DRIVERS.md D2 puts a
storage or network driver in a *cell* serving several clients, that cell's CPU budget is
consumed on behalf of others, and a client can exhaust a neighbour's guarantee through it.
Reservations (object 7) admit against a cell; the work would be happening in a different
cell.

**Does it fit?** The mechanism to fix it already exists and is unused for this: flow
context is in the ABI (docs/OBSERVABILITY.md 2) and propagates through the queue. A
request already carries who it is for. Charging follows the flow context rather than the
current cell.

**Gate.** Two clients of one driver cell, one issuing ten times the requests, with the
driver cell's charged CPU time asserted to split in proportion to *flow* rather than
landing on the driver. Buildable today with `netservice`'s fan-out shape.

### 2.6 Hardware capabilities - CHERI

**The idea.** CHERI (Cambridge/SRI, shipping as ARM Morello) makes pointers **hardware
capabilities**: bounds and permissions travel with the pointer, unforgeably, checked by
the CPU. It is the largest unlanded systems idea of the last two decades.

**Does it beat what we have?** For a different layer, yes. This tree's capabilities are
*coarse*: a cell holds a capability to an object. CHERI is *fine*: every pointer inside a
cell is bounded. The two compose - CHERI would make the uaccess seam
(`kernel/src/uaccess.rs`) enforceable by hardware rather than by a checked helper, and
would make a memory-safety bug inside a cell non-exploitable rather than merely contained.

**Does it fit?** It fits the tree's existing law exactly: probe, verify, report, fall back.
CHERI is a per-ISA capability discovered at boot, so it belongs in `arch` behind
`arch::pointer_mode()`, with the current path staying the tested default - the same shape
as FRED (docs/CPU-FEATURES.md 1.3) and for the same reason.

**Gate, and it is honest:** none available here. QEMU's CHERI support lives in a fork
(CHERI-QEMU), not upstream, and Morello is hardware. So this is designed, named, and
lab-gated - not claimed.

### 2.7 Failure as a state with a supervisor - Erlang/OTP, recovery-oriented computing

**The idea.** Erlang/OTP: processes crash, supervisors restart them according to a
declared strategy, and the *supervision tree* is part of the program's structure. Candea &
Fox's recovery-oriented computing generalised it: **make restart cheap and make it the
primary recovery mechanism**, rather than trying to make failure impossible.

**Does it beat what we have?** Yes, and this is the gap I would rank second after E2-E4.
This tree is good at *refusing* (rejections as deliverables, `DEADLOCK_EXIT` instead of a
panic, faults becoming signals) and has nothing that *restarts*. A faulted native child is
reaped with code 139 and that is the end of it. For a driver cell or a service cell -
exactly what docs/DRIVERS.md is about - "the storage driver crashed" must be recoverable,
because the alternative is that a userspace driver is *less* reliable than the in-kernel
one it replaced, which would invalidate the whole D2 argument.

**Does it fit?** Cells are already the restart unit, budgets already bound the blast
radius, and leases with **fencing tokens and epoch revocation** already exist - which is
precisely the hard part of restarting a driver safely (the old instance's in-flight DMA
must be fenced off, not hoped away). The missing piece is a declared supervision policy
and a restart verb.

**Gate.** A storage driver cell killed mid-request, restarted by its supervisor, with the
client's request either completing or failing cleanly - never silently corrupted - and the
old instance's fencing token asserted refused afterwards.

### 2.8 Host-managed flash placement - ZNS, FDP, Open-Channel

**The idea.** An SSD's FTL is a small OS the drive runs to hide its physical structure,
and it costs write amplification, unpredictable latency and over-provisioning. Zoned
namespaces (ZNS) and Flexible Data Placement (FDP) expose the structure so the *host*
places data, which it can do better because it knows the workload's lifetimes.

**Why it has only half landed.** It needs filesystem and application changes, and Linux's
generic block layer is the wrong shape for it.

**Does it beat what we have?** Yes, for the log-structured workloads named in the roadmap
(a Kafka-shaped append log is the ideal ZNS workload - append-only, known lifetime,
segment-aligned deletion). The NVMe driver exists; ZNS is a command set on top.

**Does it fit?** It fits the storage-cell design better than a monolithic kernel: a driver
cell that owns the queues can implement placement policy per client without a generic
block layer in the way. **Gate:** QEMU models ZNS (`-device nvme,zoned=on`), so this one is
*provable here* - append to a zone, assert the write pointer advances, assert an
out-of-order write is refused by the device.

### 2.9 The Arcan ecosystem, judged item by item

Arcan is worth naming because it is a rare case of a *complete* alternative built outside
the mainstream, not a paper. Its ideas were evaluated for the IPC path in
docs/LOGGING.md 0.1, and the accounting there was blunt: six of seven were already present
and one - negotiated subsegments - is weaker here than launcher-minted channels, because a
client that can negotiate for a channel is a client that can widen its own authority. One
idea, **coalescing**, was genuinely additive and is now implemented.

Three further Arcan ideas that the IPC evaluation did not cover:

- **Accessibility and debug as first-class negotiated segments.** In Arcan a client can be
  *asked* for an accessibility or debug representation of itself, and refusing is a
  visible choice. Mainstream systems bolt a11y on as an inspection API that fights the
  application. This is a genuinely good idea and it maps here without new mechanism: a
  cell's debug surface is an event stream (object 4) the launcher may mint. **Worth
  adopting**, and it composes with the reflection direction docs/REFLECTION-NEXUS.md
  describes.
- **The display server as a scriptable engine, not a policy.** Durden is a *script* over
  Arcan. Here the equivalent already exists in principle - a compositor is an ordinary cell
  (docs/DISPLAY.md, LIBRHEO.md Phase E) - so this is agreement, not adoption.
- **A12: the same client semantics over a network.** Network transparency designed in from
  the protocol rather than tunnelled. **Interesting and deliberately deferred**: it needs
  the transport stack in a cell first, and the honest sequencing is N3b/N5a before any
  remote-display protocol.

### 2.10 A queryable model of the machine - Barrelfish's system knowledge base

**The idea.** Barrelfish kept a declarative description of the hardware - topology, cache
sharing, device locations, interrupt routing - and made placement decisions by *querying*
it with a constraint solver, instead of hardcoding heuristics per platform.

**Does it beat what we have?** It sharpens something half-built. `hw::Inventory` already
holds CPU topology, NUMA regions, PCIe devices and their capabilities - the *data* is
there. What is hardcoded is the *policy*: "the pool sits 64 MiB into RAM", "a cell's home
node is round-robin", "core 0 does the global timer bring-up". The SKB idea is to make
those answers derived rather than written.

**Honest assessment: partially adopt, and resist the solver.** Deriving placement from the
inventory is right and is the direction pillar 6 already took. Putting a constraint solver
in the kernel is not - it is unbounded work in a component whose whole discipline is
bounded work and no allocation. The fit here is a *query API over the inventory*, with
policy in userspace where a solver may legitimately live.

---

## 3. Refusals, with reasons

A design is defined as much by this list. Each of these is a good idea somewhere and wrong
here:

| Refused | Why |
|---|---|
| **In-kernel bytecode VM (eBPF)** | It is a dynamic code loader in the most privileged component, which contradicts W^X being constitutional and grows the proof surface without bound. The need it serves - programmable observability - is met by userspace probes over typed event streams (docs/OBSERVABILITY.md 5), which is strictly safer and only marginally less convenient |
| **Everything-is-a-file / 9P as *the* abstraction** | Elegant, and it forces every interaction through a byte-stream funnel. Typed queue entries with per-entry grant checks are better for zero copy, better for verification, and better for a capability check that must name an *object* rather than parse a path. Per-cell namespaces - the genuinely good half of Plan 9 - are adopted (mount tables, docs/CONTAINERS-KUBERNETES.md) |
| **Unikernels as the deployment unit** | They win by deleting multi-tenancy. Cells already give the isolation and the small attack surface without giving up running several things on one machine |
| **Synchronous IPC as the primary primitive** (classic microkernel) | A syscall per message is what made microkernels slow and what io_uring later fixed by accident. Completion rings are the primary here and a synchronous hand-off is the special case |
| **Namespaces as an isolation boundary** | Shared kernel state with a filtered view; the escapes are structural. Cells are MMU- and IOMMU-enforced with their own budgets |
| **Overcommit plus an OOM killer** | Killing a process to fix an accounting error the kernel made. Budgets refuse at admission, where the caller can act (docs/MEMORY.md 7) |
| **A hard-float kernel** | Not caution: if the kernel never executes an FP instruction, no syscall, trap or interrupt has to save the vector file. The same choice Linux makes, for the same reason (docs/SUBSTRATE.md pillar 4) |
| **Formal verification of everything, before it runs** | seL4 earned its proof over years on a deliberately tiny, frozen interface. Here the discipline is *designing for* decidability - finite state transitions, no unbounded loops - plus model-checking what is integer-only (`verify/`), which is the affordable 80% |
| **Vendor-named code paths** | A guess about silicon where a CPUID bit is a fact about it. This tree has the scar: assuming x2APIC made x86 SMP and the x86 timer the same defect for months (docs/CPU-FEATURES.md 1.2) |

---

## 4. What this actually changes

Ranked by value over cost, with the sequencing constraint stated:

1. **Nothing, until E2-E4 land.** The execution entity has three representations and five
   defects came out of that (docs/EXECUTION-MODEL.md 1). Every item in section 2 that
   touches scheduling or driver cells would be built on top of it. Adding capability while
   the foundation is known-divergent is how the last five defects happened.
2. **Supervision (2.7)** - because docs/DRIVERS.md D2's entire argument is that a userspace
   driver is *better*, and an unrestartable one is not. Leases with fencing tokens already
   exist, which is the hard half.
3. **Flow-based accounting (2.5)** - same trigger: the first multi-client driver cell makes
   reservations unenforceable without it, and flow context is already in the ABI.
4. **ZNS (2.8)** - the one item in section 2 that is **provable in this container today**,
   which moves it up regardless of ranking.
5. **Channel contracts (2.1)** - fills `idl/`, and the technique (types carrying protocol
   state) is already proven one level down by `Rights<MASK>`.
6. **Persistent cross-object references (2.4)** - the analytical-DB direction's real
   unlock, gated on a proof a different cell can traverse a structure at a different base.
7. **Interference-driven core allocation (2.2)**, **CHERI (2.6)**, **A12 (2.9)** - designed
   and named, gated on hardware or on prerequisites, claimed by nobody.

## 5. The honest part

- **None of section 2 is built.** This document is reasoning plus a gate per item.
- **Three items cannot be proven here at all** and say so: interference-driven allocation
  (TCG models no cache or bandwidth contention), CHERI (upstream QEMU has no support;
  Morello is hardware), and anything needing KVM. They are designed with the fallback as
  the tested path, which is the same rule FRED follows.
- **The pillars survive this exercise unchanged**, which is the test docs/SUBSTRATE.md sets
  for itself: every item above is mechanism under the existing eight objects, and not one
  of them needs a ninth. If an idea had required a new kernel object, that would have been
  the interesting finding - and none did.
