# Scheduling

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.2; the reservation object
(section 3, object 7). See CONCURRENCY.md for strand-level (intra-cell)
scheduling and ACCELERATORS.md for the engine side.

Position: stop pretending one clever scheduler manages everything. Split into
two levels - the kernel does coarse, slow **placement** (which cores and
engines belong to which cell); userspace runtimes do fast, fine-grained
**dispatch** (which strand runs next). Context switching gets cheap mostly by
making it rare. Importance is an admission-controlled contract, never a
priority number.

## 1. No global tick

The periodic timer interrupt is a timesharing artifact for grabbing the CPU
back. Here:

- **Dedicated cores** (latency pool) run fully tickless: no timer fires unless
  a deadline is armed; the cell runs until it yields or blocks. Linux's
  `nohz_full` fights for this and never fully wins (RCU callbacks, clock
  accounting leak in); Lattice gets it by construction - there is no global
  scheduler state that must be periodically refreshed.
- **Shared-pool cores** use deadline-driven one-shot timers armed for the next
  actual event (timeslice end, lease expiry, timeout), not a fixed HZ.
- Time accounting reads timestamp counters at context-switch boundaries; the
  capability metering (CONTAINERS-KUBERNETES.md 6) reads those.

## 1a. The multicore model - multikernel, not a big lock

(Implementation is owned by docs/SUBSTRATE.md stage S3, which also names
the fair-class algorithm: EEVDF with BORE-style burstiness on top of the
EDF reserved class. What is being scheduled - the **execution entity**, its
information budget, its state machine and its invariants - is designed in
docs/EXECUTION-MODEL.md, along with the core-class taxonomy this section's
pools bind to: P / E / LP cores and accelerator engines are one taxonomy, so
placement is one decision rather than a scheduler plus a separate engine path.)

A choice the design implied but never stated: **the kernel control plane is
per-core (multikernel-style), not one shared kernel behind a big lock (SMP).**
Each core runs its own kernel instance with its own scheduler state, its own
run queues, and its own view of the objects it owns. Cores share no implicitly
mutable kernel state. Cross-core communication is *explicit*, using the same
ring + doorbell mechanism as everything else, with an inter-processor interrupt
(IPI) as the hardware doorbell for the cross-core case.

Why this and not SMP-with-a-big-lock:

- **The big kernel lock is exactly the shared mutable state the whole design
  avoids.** An SMP kernel serialises syscalls through a lock, which slows them
  and destroys determinism - the opposite of the tickless, per-core,
  contention-free model above. There is already "no global scheduler state
  that must be periodically refreshed" (§1); a shared kernel lock would
  reintroduce it.
- **Verification survives partitioning.** A per-core kernel is, from the
  verification standpoint, much closer to the single-core case: another core
  running its own kernel is like another bus master, and correctness reduces
  to *partitioning* being correct rather than to reasoning about concurrent
  access to shared kernel structures. SMP concurrency is the thing that is
  hardest to verify and most prone to subtle, hard-to-spot bugs; the
  multikernel model keeps the capability-core proof (KERNEL-RUST.md 5)
  tractable per core.
- **It is the same continuum as distribution.** "Local and distributed are one
  mechanism" (ARCHITECTURE.md 2) extends *below* the node boundary: core-to-core
  is simply the shortest, fastest link on the same continuum as node-to-node.
  Core-to-core uses IPI + shared memory; node-to-node uses RDMA or QUIC. The
  cell, capability, queue, and flow-ID abstractions are identical at both
  scales. A graph edge between two nodes on different cores and a graph edge
  between two nodes on different hosts differ only in transport and latency,
  not in mechanism.

### Cross-core communication is the async ABI, not cross-core calls

This is where the seL4 community's hard-won lesson (their multikernel
discussion) directly confirms Lattice's ABI bet. Working *forward* from a
synchronous-PPC-first kernel, seL4 practitioners concluded that for cross-core
work you should abandon synchronous cross-domain calls and use **a message
queue plus async notification/IPI** instead - proxying synchronous calls
across cores is a "slippery slope" that is both slow and a security hazard,
and cross-core synchronous calls are slow even under SMP. Lattice's universal
ring + doorbell ABI (IO.md 6.1) *is* that recommended model, made the default
everywhere. The synchronous PPC that seL4 does better than Lattice on a single
core (IO.md 6.1's honest concession) is precisely the primitive that does not
cross cores well - so Lattice's async ABI is not only a throughput choice, it
is the model that already degrades gracefully across cores and nodes without a
proxy layer.

### The honest hard part - shared hardware

A multikernel does not make physically shared hardware disappear. The
interrupt controller (GIC on ARM, IOAPIC/x2APIC on x86), last-level caches,
and memory controllers are shared silicon even when each core runs its own
kernel. Partitioning them is a discipline the kernel and boot path must
*enforce*, not a property the hardware guarantees - and getting shared
interrupt-controller routing right across per-core kernels is the specific
place where naive multikernel setups "work mostly by chance." Lattice's
requirements:

- The boot path (BOOT.md) assigns each core its owned interrupt lines and
  programs the interrupt controller's routing explicitly; no line is delivered
  to a core that does not own its handler.
- IPI vectors are a managed, capability-gated resource: a cell cannot cause an
  IPI to a core it has no channel to, because the cross-core doorbell is
  minted from a queue-pair capability like any other.
- Shared LLC and memory-controller interference is handled by the existing
  interference-control machinery (§9) - cache partitioning (CAT/MPAM) and
  memory-bandwidth allocation are per-core-kernel policies, not left to chance.
- x86 IPI support is called out as a real engineering cost: unlike ARM GICv2/v3
  and RISC-V (SBI), x86 cross-core signalling needs more work in the boot and
  interrupt layers. The Arch trait (TARGET-ARCHITECTURES.md) is where this
  per-ISA difference lives.

This is the one part of the multicore model that is genuine, unavoidable work
rather than a clean consequence of the doctrines, and the validation suite
must exercise cross-core partitioning under interrupt storms and interference
(a target for the P9/P31-class interference gates, VALIDATION.md).

## 2. Pools

- **Latency pool:** dedicated tickless cores, no preemption, SMT off - the
  cell runs until it yields. What people fake with `isolcpus` + IRQ affinity
  + DPDK, native here.
- **Shared pool:** timesliced cores with an EEVDF-style fair scheduler for
  batch and control-plane work.
- **System pool:** cores, memory, and queue capacity carved out at boot
  (BOOT.md 3) that tenant admission can never hand out. Under total overload,
  tenants degrade; the control plane provably does not.

## 3. Two-level scheduling

- The kernel grants vcores to a cell; the cell's runtime schedules strands on
  them (CONCURRENCY.md). The runtime already does this today - it just fights
  the kernel scheduler underneath it, which double-schedules and migrates
  behind its back.
- The kernel notifies the runtime when it takes a vcore away or a blocked
  operation completes (via the async queues). This is **scheduler
  activations**, which failed in the 1990s (NetBSD and FreeBSD removed it)
  because it was complex bolted onto POSIX blocking threads. It is viable
  here for one reason: **there is no blocking syscall to reconcile** - the
  entire syscall model is async queues. Google's ghOSt (pluggable userspace
  scheduling for Linux) is evidence the demand is real.

## 4. Real time - contracts, not priorities

- A **reservation** requests `(budget, period, deadline)` - e.g. "2 ms of CPU
  every 10 ms, done within 5 ms of arrival."
- **Admission control** accepts a reservation only if the schedulability math
  holds on the target cores (EDF utilization bounds per pool). Accepted means
  *guaranteed*. This is Linux's `SCHED_DEADLINE` promoted from a corner to
  the primary RT interface, with three upgrades:
  1. Reservations are **capabilities** - minting them needs a grant, budget-
     bounded per tenant. Nobody `nice -20`s themselves into importance.
  2. **Budget overrun is throttled, not trusted** - exhaust your slice and
     you are descheduled until the next period, so an RT cell cannot starve
     the machine (Linux's 95% RT-throttle is a crude global version).
  3. **Hard vs soft is explicit:** hard reservations get dedicated tickless
     cores with SMT off and are *rejected* if topology cannot guarantee them;
     soft reservations timeshare with EDF and degrade proportionally.

### 4.1 Reservation classes — four tiers

```rust
pub enum ReservationClass {
    Hard,      // guaranteed, admission-checked, never revoked, dedicated cores
    Soft,      // guaranteed budget, degrades proportionally under pressure
    Elastic,   // floor + reclaimable ceiling (pressure events reclaim the ceiling)
    Residual,  // NO guarantee; consumes only slack cycles the pools leave idle
}
```

The first three are the reservation model above. The fourth, **Residual**,
handles work that is legitimately unschedulable as a reservation yet must
eventually run: background indexing, log rotation, nightly reports,
spare-cycle batch jobs. Pure reservation would reject such work (it has no
deadline to admit against); a priority scheme would risk starving it or
letting it starve others.

Residual work:
- **Is admitted always** — it promises nothing, so there is nothing to check.
- **Consumes only slack** — it runs when a pool would otherwise idle
  (including the latency pool's 10% safety margin and the shared pool's unused
  capacity) and yields at its next yield point the instant any reserved work
  becomes runnable.
- **Is metered** so a runaway residual job is visible, but it definitionally
  cannot starve reserved work because it is always preempted first.
- **Makes only statistical progress guarantees** ("completes when the system
  has spare capacity") — the honest contract for best-effort work.

This is not priority in disguise: there is exactly one residual tier, it
cannot be tuned to compete with reserved work, and reserved work is never
scheduled *against* it — residual simply mops up slack. It closes the
best-effort gap without reintroducing priority-inversion pathologies. See
REFLECTION-NEXUS.md §3 for the reasoning.

## 5. "The database is the most important" - how it is said

Not a priority: a **resource contract** on the database's cell group -
dedicated cores from the right NUMA domain, a *reserved* (non-reclaimable)
memory grant so there is no OOM roulette, reserved NVMe queue depth, and I/O
bandwidth shares. Importance is a set of enforceable reservations across CPU,
memory, and I/O. Linux needs cgroups + cpuset + ionice + mlock + oom_score_adj
stitched together to approximate this; here it is one admission-checked
contract.

## 6. NUMA - threads follow memory

A cell is born with a home NUMA domain; its allocations and cores come from
that domain by default. Work-stealing is **topology-bounded** - steal within
your domain freely, cross domains only past a load threshold. This flips
Linux's model (threads bounce first, NUMA balancing migrates pages after the
damage). Scheduling and memory placement read the same topology graph and
make one joint decision (MEMORY.md 4).

## 7. GPU/NPU - schedule space, not time

Accelerators preempt poorly, so time-slicing is the last resort. First choice
is spatial partitioning; the kernel arbitrates at command-buffer granularity
and enforces budgets; unbounded kernels are budget-killed, not preempted.
Full treatment in ACCELERATORS.md 3.

## 8. SMT

Sibling hyperthreads share caches, ports, and side channels. Rule: only
strands from the **same cell** may share a physical core (Linux calls this
core scheduling; mandatory here). Latency-pool cores disable SMT outright -
a dedicated core with an unpredictable sibling is not dedicated.

## 9. Interference control

Cache and memory-bandwidth contention between co-located cells is not fully
capturable by CPU-time accounting. **RDT/CAT+MBA** (x86) and **MPAM** (ARM)
partition cache and bandwidth; the topology graph exposes the partitions, and
the admission math for shared-pool reservations accounts for them. Hard-RT
co-located with a bandwidth-thrashing neighbor is still physics - which is
why hard reservations get dedicated cores.

## 10. Deadline inheritance across dependency graphs

**The problem:** an RT strand waiting on a slow service (A waits on B's
completion, B is low-budget) needs the deadline to flow along graph edges,
not just along mutexes. Priority inheritance on locks is solved
(CONCURRENCY.md); the graph-edge case needed an answer.

**The answer — budget propagation along the flow ID (adapted from seL4).**
seL4 solves the synchronous version of this with *scheduling-context
donation*: on a protected procedure call, the callee runs on the caller's
scheduling context (its time budget), so a server automatically inherits the
urgency of whichever client called it — no separate priority-inheritance
protocol, no server-priority guessing. The server has no budget of its own;
it always executes against the budget of the request it is serving.

Lattice's graph edges are asynchronous, so a scheduling context cannot be
donated hand-to-hand across a blocking call the way seL4 does it. But the
*principle* transfers directly: a graph node servicing a request should
execute against the **originating reservation's budget**, not a generic
server budget. The mechanism is the flow ID (OBSERVABILITY.md): the
originating reservation is carried in the flow context that already
propagates along every graph edge. When node B activates to service a
request stamped with flow F, the scheduler charges B's execution to F's
reservation and schedules B at F's deadline — B inherits the urgency of the
request, exactly as an seL4 server inherits its client's scheduling context.

Concretely:
- Each reservation has a scheduling-context handle.
- The flow context carries the originating reservation's scheduling-context
  handle (not just the trace ID).
- A service cell that processes work for many clients does not hold its own
  RT budget; each unit of work runs on the budget of the flow that submitted
  it. A high-priority request and a background request to the same service
  are scheduled at *their own* deadlines, not the service's.
- This composes across hosts: the flow's scheduling context travels with the
  cross-host message; a remote node servicing an RT request runs that work at
  the request's deadline (bounded by the lease TTL, which remains the failure
  mechanism if the remote node cannot meet it).

This removes the server-priority-assignment problem entirely (a service does
not need a priority; it borrows the caller's), and it removes priority
inversion at the graph level (a low-urgency request cannot delay a
high-urgency one through a shared service, because they run on separate
donated budgets). It is seL4's budget-donation insight, re-expressed for an
async, flow-tracked, dependency-graph world.

**Honest limit:** donation across an async edge is not free the way seL4's
synchronous hand-off is — the scheduler must look up and switch to the flow's
scheduling context when B activates, which costs a few tens of nanoseconds
over running on a static server budget. For the RT paths where this matters
the cost is worth the guarantee; for bulk best-effort work the service can
opt to run on its own Residual budget instead (§4.1) and skip the lookup.

## 11. Learnings from production Linux schedulers (CachyOS)

CachyOS is the performance-tuned Linux distribution whose whole identity is the
CPU scheduler, so it is the best available evidence for what the choices in this
doc cost and buy in practice. Its stack is three layers, and each one maps
directly onto a decision already made above - which is the point of recording it:
the mainstream performance community arrived, working *forward* from CFS, at the
model this doc reached working *backward* from the reservation contract.

### 11.1 What CachyOS actually ships

- **EEVDF** (Earliest Eligible Virtual Deadline First) - the mainline default
  since Linux 6.6 and CachyOS's base. Each task is given a **virtual deadline** =
  its eligible time + `slice/weight`, and an **eligibility** gate: a task may run
  only once its *lag* (fair share received minus consumed) is non-negative. Among
  eligible tasks, the earliest virtual deadline wins. The load-bearing property:
  a task that requests a **smaller slice gets an earlier deadline**, so a
  latency-sensitive task is served first *without any priority knob* - latency is
  bought by asking for less, not by ranking higher.
- **BORE** (Burst-Oriented Response Enhancer, Masahito Suzuki) - a heuristic *on
  top of* EEVDF, not a replacement. It tracks each task's **burst time**: the CPU
  time consumed since the task last *voluntarily relinquished* the CPU (sleep,
  I/O-wait, or `yield`). It turns that into a **burst score** by taking the
  **bit-length of the normalized burst time** - a cheap **integer log2** - then
  applying an offset (`penalty_offset`, default 24 bits subtracted) and a scale
  (`penalty_scale`, default 1536 = 1.5x in 1/1024 units). The score lands in
  **0..39, exactly like `nice`**: each step of -1 grants ~**1.25x** more timeslice
  and more wakeup-preemption aggressiveness. Suzuki frames it as a *radix
  conversion from binary-log to common-log* - mapping a nanoseconds-to-minutes
  burst range onto a ~0.01-100x weight, dimensionlessly. So greedy tasks (long
  runs between yields, usually CPU-bound batch) are weighted down and modest tasks
  (short runs, usually I/O-bound interactive) are weighted up, reaching an
  equilibrium. Two refinements matter: a **forked child inherits an ancestor's
  average child-burst** (a hub/stub topological walk that skips single-child
  nodes) so a `make` spawning CPU-hungry children cannot swamp interactive tasks;
  and the score is **EMA-smoothed** against a historical score (`take the larger of
  latest or history`) to survive burst spikes. Pure inference from observed
  behaviour, **integer-only**, no new mechanism.
- **sched_ext (scx)** - `CONFIG_SCHED_CLASS_EXT`: a scheduler *class* whose policy
  is a **BPF program loaded (and swapped) at runtime**, usually with a Rust
  userspace half. CachyOS enables it by default and ships a GUI to switch policies
  live. The notable ones: **scx_lavd** (Latency-criticality Aware Virtual Deadline
  - estimates each task's latency-criticality from its wakeup/run behaviour and
  scales its virtual deadline by it; the default on CachyOS's handheld/gaming
  build), **scx_flash** (an EDF/deadline scheduler balancing latency and
  fairness), **scx_rusty** (per-LLC multi-domain load balancing, most logic in
  Rust), **scx_bpfland** (prioritises tasks that yield the CPU voluntarily).

### 11.2 sched_ext is the two-level bet, validated - and it supersedes ghOSt

§3 justified two-level scheduling and scheduler activations by pointing at
Google's ghOSt as evidence "the demand is real." sched_ext is that demand landed
in the mainline kernel: scheduling **policy in userspace, mechanism in the
kernel**, swappable at runtime without reboot - exactly the mechanism/policy split
this whole design rests on (ARCHITECTURE.md 4.7). The lesson is not "add BPF"; it
is that the shared-pool **dispatch policy must be a single swappable seam**, not a
hardcoded scheduler baked into the kernel. Lattice already puts fast dispatch in
the userspace runtime (§3); the kernel-side placement policy for the shared pool
should be equally pluggable - one `Policy` seam the boot path selects, the same
way the `net` crate selects `hft`/`edge`/`warehouse`/`embedded` (NETSTACK.md).
CachyOS proves the payoff is real: LAVD for interactive/handheld, flash for
latency+fairness, rusty for throughput servers - **one workload, one policy**, not
one scheduler pretending to fit all.

### 11.3 EEVDF's virtual deadline unifies with our reservations - one ready order

The keystone takeaway. This doc already schedules **reserved** work by EDF
(§4: `SCHED_DEADLINE` promoted to the primary interface) and has the deadline
substrate for it - the kernel timer arbiter (NETSTACK.md 16, the single owner of
the per-ISA one-shot) plus the reservation admission math (`kernel/src/sched.rs`).
EEVDF shows that **best-effort work belongs in the same deadline-ordered
structure**: give each non-reserved task a *virtual* deadline (`eligible +
slice/weight`) and the shared pool becomes one ready queue ordered by deadline,
with reserved cells carrying *hard* deadlines and best-effort cells carrying
*virtual* ones. That is "compose before extending" (ENGINEERING.md): no second
scheduler, no priority axis - Residual work (§4.1) is simply the tail of the same
order (a virtual deadline at +infinity, run only on slack). It also means Lattice
should **not** port CFS-style fair timesharing as a separate thing; it is already
closer to scx_lavd/scx_flash (deadline schedulers) than to CFS, because it started
from deadlines. Build the shared-pool scheduler as *virtual-deadline EEVDF over
the existing arbiter*, not as a fairness engine bolted beside the reservation one.

### 11.4 BORE and LAVD are observe-never-infer, applied to time

Both infer a task's urgency from **measured behaviour** - burst length, wakeup
frequency, how often it yields - never from a number the task declares. That is
ENGINEERING.md's observe-never-infer rule and this doc's own "importance is a
contract, never a priority number" (position statement), reached independently by
the interactivity community. Lattice is positioned to do the *honest* version:
the per-context blocking work (LINUX-COMPAT.md L4, `thread.rs` `pblock`) already
records exactly the evidence BORE and LAVD estimate from - when each context
blocks (voluntarily relinquishes), on what, and how often it is woken. BORE's
**burst time is precisely "cycles since this context last blocked"**, which the
`pblock` machinery already marks; a burst score follows directly as
`bitlen(burst_cycles >> offset)` scaled to a small integer - and because BORE's
score is a **bit-length (integer log2)**, not a float, it lands natively in this
kernel's no-FPU, fixed-point discipline (the `sched.rs` parts-per-million
precedent), unlike a CFS-style `vruntime` that wants division. That integer burst
score is the *weight* term of the §11.3 virtual deadline (`eligible +
slice/weight`): a short-burst, frequently-woken context gets a smaller effective
denominator → earlier deadline → served first, with **no task-declared hint to be
lied to** (observe-never-infer). BORE's fork caveat transfers too: a cell that
spawns many CPU-hungry children (`fork`/`SYS_SPAWN`, a build) must not let those
children swamp interactive cells, so a spawned cell should **inherit its parent's
observed burst** as its starting score rather than a neutral default - the
hub/stub inheritance idea, expressed over the cell tree this OS already has.

### 11.5 What Lattice deliberately does not take

- **Cross-LLC load balancing / work-stealing schedulers** (scx_rusty's core
  competency). The multikernel model (§1a) *partitions* cores rather than
  balancing across a shared runqueue, and NUMA work-stealing is already
  topology-bounded (§6). A global balancer is the shared mutable scheduler state
  §1a exists to avoid; CachyOS needs it only because Linux is one SMP kernel.
- **A periodic-tick fair scheduler as the default.** EEVDF/BORE still run under a
  timeslice tick; §1 removes the global tick, so the shared-pool EEVDF here is
  driven by the deadline arbiter's one-shots, not HZ.

### 11.6 Honest gate - none of this is implementable yet

Every technique above is **preemptive and multi-core**. Lattice today is
**cooperative and single-CPU**: a context yields only at a syscall boundary
(CONCURRENCY.md, LINUX-COMPAT.md L4), and SMP bring-up (docs/SMP.md) proves a
second core runs but does not yet schedule on it (#27/#132). So:

- The virtual-deadline shared pool (§11.3), the behaviour-inferred criticality
  score (§11.4), and the pluggable `Policy` seam (§11.2) are **design targets
  gated on preemption + SMP**, recorded here so the scheduler is built toward them
  rather than retrofitted.
- The cooperative pick order is **deliberately left round-robin for now**. Making
  it latency-aware would change nothing measurable under a single CPU with no
  preemption (a compute-bound context still holds the CPU until it yields) and
  would break the deterministic ordering the proofs assert (`schedidle`'s
  `bSSSSSSSSB` oracle, `netservice`'s `order == [0,1,2,...]` interleave witness).
  The payoff arrives with preemption, and so does the change - not before.

Sources: the **BORE scheduler** README (`github.com/firelzrd/bore-scheduler`,
Masahito Suzuki - the algorithm, tunables and defaults in §11.1 are quoted from
it), the CachyOS sched-ext wiki (`wiki.cachyos.org/configuration/sched-ext`), the
`sched-ext/scx` scheduler repository, and the EEVDF/ghOSt background in kernel
documentation and LWN.

## 12. Heterogeneous cores: capacity, class, and Intel Thread Director

P-cores and E-cores execute the **same** instruction set (docs/RESOURCE-GRAPH.md 2.4b), so
nothing here is about capability and everything is about *capacity*. `sched::hetero` holds the
per-CPU capacity table and the three decisions that read it.

**Classification is observed, not inferred.** Intel Thread Director is hardware that watches a
running thread - IPC, cache misses, memory stalls, vector mix, spin behaviour - and hands the OS a
per-thread class. Where it exists it is the better source and the hint records that
(`HintSource::ThreadDirector`). Where it does not, the substitute is not a heuristic: **every
relinquish in this OS is an explicit, counted transition through a named call** (`sched::bore`), so
"how long does this entity run before it voluntarily gives the CPU up" is a measurement rather than
the inference Linux's CFS had to make - and that is precisely the signal Thread Director's
`compute intensive` versus `mostly sleeping` hints carry. What is genuinely missing without the
hardware is the *microarchitectural* half - IPC, stalls, vector mix - which needs a PMU, is modelled
by no emulator here, and is named as absent rather than approximated.

Three classes, each derivable from what is actually observed:

| Class | Observed as | Placed on |
|---|---|---|
| `Unknown` | has never relinquished - a freshly created entity | the **fastest** core. An unknown demand over-served costs a little energy; under-served it costs a migration *and* the time already lost |
| `Compute` | has relinquished, and its burst score is at or above the threshold | the **fastest** core - throughput |
| `Bursty` | has relinquished, short bursts - an event loop, a shell, an I/O-bound strand | the **slowest** core, leaving the fast ones for work that can use them. Its latency comes from being *dispatched* promptly, which the burst-weighted virtual deadline already gives it (low score = high weight = earlier deadline) |

**Fairness is deliberately *not* rescaled by capacity.** It would be easy to charge virtual time
by delivered work rather than by wall-clock, so that a task on an E core is credited less. That
changes what fairness *means* - Linux keeps vruntime wall-clock based on purpose and uses capacity
for placement and utilization instead - and re-deciding the fairness definition is not something a
capacity feature should do as a side effect. Capacity affects placement, steal direction and the
reported statistics; the EEVDF ordering is untouched.

**A mismatched steal is counted, never prevented.** Work conservation wins and the crossing is
reported, the rule the locality work already holds. And this preference is real even for work that
has **not started yet** - which is what distinguishes it from the cache-domain steal preference
docs/RESOURCE-GRAPH.md 6.3a refuses: a cache domain is about moving a working set an unstarted
entity does not have, where capacity is about how fast it will run for its whole life once it does.

**A uniform machine must be unaffected**, and that comes out of the *tie rule* rather than a
special case: "highest capacity, ties to the lowest CPU number" is already "the lowest CPU number"
when every capacity is equal. A first version checked a `is_hybrid()` gate there, the check was
observed to change no answer, and it was removed - one path cannot drift away from the case it was
meant to preserve.

**Placement is wired into the multi-core claim**, not only modelled. `smp::place_cells_classed`
publishes a class per cell and `claim_matching_tier` scans for work whose class suits the claiming
core's tier before falling back to the ordinary cursor. Its safety is the same as `steal`'s - the
`PLACE_RUN` exchange, which exactly one core can win - so a scan is as exclusive as a cursor. It
claims **unclaimed** work only; taking a peer's claim is a steal, which has its own path and its own
counter, and conflating them would report a preference as a rebalance.

**A core claims one cell at a time on a hybrid machine** (`CLAIM_BATCH_HYBRID`), and that is a
design statement rather than a test convenience: a batch is a core holding work it has not started,
and on a hybrid machine some of it may not suit that core's tier while a core that does suit it sits
idle. The cost is the one batching exists to avoid - a core holding one cell has nothing to preempt
*to* - and the trade is the right way round, because a mis-tiered cell runs slowly for its whole
life where a missed preemption costs one slice. **Observed**: with the batch restored to two, the
`smp` phase fails on some runs and passes on others, and that intermittency *is* the finding.

Proven by `verify/hetero/` (8 deterministic properties + 20,000 random machines, 1..8 cores of
random class and capacity, oracle by scanning the declared table), by `hwinfo`'s assertion that the
discovery ran and honestly found nothing, and by the `smp` kernel on all three ISAs: CPUs 0-1
declared `Performance` and 2-3 `Efficiency`, two compute and two bursty cells published, and every
compute cell asserted to have run on a full-capacity core and every bursty one on a reduced one,
with all four claims through the preference and zero tier crossings. The machine is restored to
uniform before the assertions, so a failure cannot leave a declared asymmetry behind for a later
phase.

One defect on the way, worth keeping because of how it presented: the queue is republished **grouped
by home node**, so slot `k` is not the caller's cell `k`, and reading the class by slot told the
preference the wrong thing about the cell - a compute cell landed on an efficiency core with the
mechanism working perfectly. The class is looked up through `PLACE_ORIGIN` now. Honest about that
one's proof: it was found by the phase failing on its first run, and a re-inserted control does not
reliably fire, because whether slot order differs from caller order depends on the home nodes those
four cells happen to draw.

Six controls firing across the two layers; two that did **not** fire are recorded rather than
dropped - `pick_cpu`'s uniform gate (redundant against the tie rule, so it was removed) and the
`PLACE_ORIGIN` lookup (intermittent by construction).

## 12a. Honest costs

- Partitioning wastes cores at low utilization; fair timesharing is *better*
  for a laptop. This design assumes a server whose workload mix is known and
  provisioned.
- Two-level scheduling makes runtimes more complex and debugging harder
  (mitigated by mandatory runtime introspection, OBSERVABILITY.md 7).
- Spatial GPU partitioning fragments capacity.
