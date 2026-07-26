# I/O

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.4 and 4.6, and the queue
object (section 3, object 3).

One model for all I/O: **submit descriptors, receive completions, move
ownership instead of bytes**. Disk, network, GPU, IPC, and timers are the
same machinery at different endpoints. Blocking does not exist below the
library level.

## 1. The queue ABI

- A queue pair = submission ring + completion ring in shared memory + a
  doorbell. Entries are fixed-layout `repr(C)` structs (the Tier-1 ABI):
  version field, opcode, flags, capability references, 16-byte flow context,
  user data. No parsing, no varints - decode is a pointer cast plus
  validation, and the validators are continuously fuzzed.
- **Doorbell coalescing:** submit N entries, ring once. Completions arrive
  batched; one wakeup can carry 64+ completions. The fast path of a hot
  server amortizes kernel involvement to fractions of a syscall per
  operation.
- Wakeup policy per queue: interrupt-driven (delivered as events) for
  latency queues, polled by dedicated-core strands for throughput queues -
  a grant attribute, not a driver mystery.
  - *Implemented so far (docs/LIBRHEO.md Phase D/F):* the **first two hardware
    interrupts**. A native cell blocking on console input (`SYS_WAIT_INPUT`)
    parks, and the kernel idles at `wfi`/`hlt` until the **UART RX interrupt**
    delivers a byte (RISC-V S-mode external via the AIA IMSIC; ARM64 PL011 SPI
    via the GICv3; x86-64 still polls - its QEMU TCG split-irqchip IOAPIC/LAPIC
    does not re-deliver reliably). A cell blocking on a deadline (`SYS_ARM_TIMER`)
    parks until the **timer interrupt** fires (RISC-V Sstc `stimecmp`; ARM64 CNTV
    virtual timer via the GICv3) - a genuine 0%-CPU park on those two ISAs;
    x86-64's LAPIC one-shot is driven over an x2APIC MSR block that QEMU TCG leaves
    inert, so it falls back to a cooperative deadline check (verified at bring-up,
    docs/NETSTACK.md 16 Phase N2h). Every deadline goes through the kernel **timer
    arbiter** (`kernel/src/ktimer.rs`), the single owner of the one-shot. The general
    per-queue completion IRQ is future work.

## 2. Completion contracts

Every submission declares *(what must be true at completion, how long the
system may wait to say so)*:

- **Durability classes** for storage: `volatile` (in cache), `ordered`
  (persist-before-X barriers without flushing - the semantic fsync always
  conflated), `durable` (completion fires only when persistent), plus
  `durable-local` vs `durable-replicated` so commits can pipeline honestly.
- **Windows** turn latency slack into batching the system harvests
  automatically: a thousand strands each writing one record with 1 ms slack
  become one group-commit flush without knowing about each other. Linux
  batches by heuristic (writeback timers, plugging, dirty ratios); Lattice
  batches by contract.
- Windows are admission-controlled like reservations: accepting
  `durable <= 50 us` beyond what the log device sustains is the same lie as
  over-admitting real-time budgets, and the same math refuses it.

## 3. Streams and zero copy

- A stream = a descriptor ring over a **per-stream arena** from the owner's
  grant. Payload is written once into the arena; a 32-byte descriptor
  (offset, length, arena reference, flags) travels. Consumers read in place.
- **Scatter-gather lists are the universal chunk currency.** Producer and
  consumer grain mismatch (NIC frames vs 64 KB logical records vs 2 MB GPU
  tiles) is resolved by descriptor arithmetic, not memcpy. NICs, NVMe
  (PRP/SGL), and DMA engines consume gather lists natively, so an SG chunk
  can leave via another device without ever being flattened.
- When contiguity is genuinely required, the re-layout is **one explicit DMA
  gather node** in a dependency graph - off-CPU, scheduled, visible in the
  trace. Doctrine: zero CPU copies always; zero total copies where hardware
  permits; unavoidable copies are graph nodes you can count.
- **Seal:** producer fills, kernel flips write permission, buffer becomes
  immutable. N readers map read-only, validate once, trust forever; device
  mappings are read-only too. Shared-memory TOCTOU is dead by construction.
  Seals batch per slab (one TLB shootdown for many chunks).
- **Inline threshold:** below ~1-4 KB (measured per platform, a system
  constant) payloads are copied inline in the entry - a small copy beats any
  page-table game. Above it, sealed-by-reference.
- Arena geometry (alignment, size classes, header/payload split) is
  negotiated once at stream connect, not fought per chunk.

## 4. Backpressure and failure

- Every ring is bounded; full is an explicit condition, never invisible
  buffering. Credits are denominated in **bytes**, so mixed-grain producers
  and consumers meter in one unit.
- Slow consumers hold **reader leases** on sealed chunks: a stalled consumer
  loses its mapping at lease expiry, and stream policy chooses drop, stall,
  or snapshot semantics per stream class.
- Peer death, revocation, and pressure arrive on the same completion rings
  as success (doctrine 7). Timeouts are part of an operation's type, not an
  application afterthought.

## 5. Storage I/O specifics

- NVMe queues are granted to storage-engine cells like NIC queues to network
  cells; the kernel never sits in the data path.
- The path from "load model shard" to bytes-in-HBM is one dependency graph:
  NVMe read nodes -> (optional NIC RDMA nodes) -> HBM write nodes, peer-to-
  peer where hardware allows, one flow ID end to end (AI-ARCHITECTURE.md 3).
- The page cache as invisible kernel magic does not exist; caching is an
  explicit, capability-visible tier in the typed-memory system, and
  `streaming` intent bypasses it outright - no O_DIRECT decades-long
  argument.

## 6. IPC as I/O

Cross-cell calls are the same ABI: connect = capability exchange yielding a
typed queue pair whose protocol (request/response, stream, one-way) is
declared in the IDL. Cross-host, the identical typed protocol rides RDMA or
QUIC transports chosen at connect time - location-transparent in mechanism,
visible in cost (doctrine 9).

### 6.1 What the queue ABI is, and what it trades away (from seL4)

The queue pair is best understood as an opinionated composition of two of
seL4's three cross-domain channels: the submission/completion rings are
seL4's **data channel** (shared memory the kernel never copies through), and
the doorbell is seL4's **signaling channel** (an async notification that the
ring has entries). seL4 provides these as separate primitives and lets each
subsystem compose them; Lattice bakes the ring+doorbell composition in as
*the* universal ABI, because for its target workloads (AI, storage,
networking, fleet - throughput- and data-plane-heavy) that composition is
almost always the right one, and standardising it is what enables flow-ID
tracing, completion-window batching, the dependency-graph model, and one
uniform engine abstraction.

The honest trade-off: seL4's third channel is the **control channel** -
the Protected Procedure Call (a synchronous, directed context switch into
another protection domain with register-passed arguments and a return). For
a fine-grained synchronous cross-domain call ("do X in that component and
give me the result now"), seL4's PPC is near-optimal and genuinely *faster*
than a ring+doorbell round trip, which needs a submit, a signal, a wakeup,
and a completion where the PPC needs one directed switch. Lattice
deliberately de-emphasises this path: a synchronous request/response over a
queue pair is expressible, but it is not the privileged primitive, and it
carries the round-trip cost rather than PPC cost.

This is a workload-profile judgment, not a claim that async is universally
superior. Lattice bets its targets are dominated by the data+signaling
pattern - where it matches seL4's own approach and adds batching, tracing,
and graph structure on top - and that the synchronous fine-grained
control-transfer pattern (where seL4's PPC wins, and which is central to the
small-cooperating-component systems seL4 was built for) is rare enough in
Lattice's target profiles to not deserve first-class ABI status. Where that
bet is wrong - a workload genuinely dominated by tiny synchronous cross-domain
calls - seL4's model is the better fit, and this should be stated plainly
rather than papered over. The P2 gate (§7) tests the single-message round
trip precisely because that is the number where this trade-off is visible.

## 7. The three numbers

A control-plane kernel lives or dies on grant-check latency, queue round
trip, and context switch. These carry permanent perf-regression gates and
the kill thresholds in ARCHITECTURE.md 8.4 (P1-P6).
