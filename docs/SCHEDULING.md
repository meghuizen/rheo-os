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
EDF reserved class.)

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

## 11. Honest costs

- Partitioning wastes cores at low utilization; fair timesharing is *better*
  for a laptop. This design assumes a server whose workload mix is known and
  provisioned.
- Two-level scheduling makes runtimes more complex and debugging harder
  (mitigated by mandatory runtime introspection, OBSERVABILITY.md 7).
- Spatial GPU partitioning fragments capacity.
