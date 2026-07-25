# Concurrency - Threads, Strands, and Locking

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.3; pairs with SCHEDULING.md
(which schedules the vcores these strands run on).

**Implemented (sections 1, 6):** the `runtime/` crate has the strand executor
- strands are `Future`s, "blocking" is a park on a token, and the queue-pair
completion carrying that token in `user_data` is what unparks the strand (one
drained completion ring, N strands resumed). On top: an async channel,
`spawn`/typed `JoinHandle` (structured concurrency), `yield_now`, an async
`Mutex` that parks on contention (section 6 - the vcore is never lost to a
held lock), and a fair `TicketLock` for the future multi-vcore case. Proven on
all three ISAs by the `runtime` test kernel (single-vcore, kernel-context).
Validated as light threads against Linux/Go/Python in comparison/threads/
(~85 ns spawn+teardown, ~12 ns switch; ~1,200-1,600x faster than OS threads,
~8-17x faster than goroutines). Deferred: multiple vcores + granting, the
preemption doorbell (section 4, needs the timer/IRQ path), stackful strands
(section 2), priority-inheritance locks, and vcore-local storage.

Position: threads get light by splitting in two. The kernel schedules
**vcores** (one kernel context each); the runtime inside a cell schedules
**strands** - user-level threads costing ~200 bytes, spawned in the
hundred-thousands, switched in tens of nanoseconds without entering the
kernel. Blocking does not exist, so a strand never loses its vcore to a
hidden block. Every classic threading bug gets a structural answer, not a
debugging tool.

## 1. The strand model

- A cell holds N vcores. The kernel knows exactly N contexts - not the 100k
  strands on them. Strand create/switch/join/park is pure userspace: no
  syscall, no kernel memory, no global thread table. Goroutines/Tokio-tasks,
  but with the OS designed for it.
- A strand that "blocks" (I/O, lock, channel) pushes a submission entry,
  parks, and the runtime runs the next strand. The vcore never idles into the
  kernel while runnable work exists. Because every kernel interaction is an
  async queue, the killer of M:N threading - a hidden blocking syscall
  silently eating a core - cannot happen. This is why scheduler activations
  failed on POSIX and works here (SCHEDULING.md 3).
- Completions return with a strand ID in the user-data field; the runtime's
  poller unparks exactly that strand. One kernel notification carries a batch
  of completions - one wakeup, N strands resumed.

**Status - the "kernel schedules N contexts per cell" half is real for Linux
cells (docs/LINUX-COMPAT.md L4).** The Linux personality realizes the vcore
mechanism directly: a Linux cell holds up to 8 execution contexts (a TrapFrame +
FP save area each), scheduled **cooperatively, round-robin, at syscall
boundaries** on the single CPU (`kernel/src/linux/thread.rs`). `clone` creates a
context, `futex` WAIT/WAKE is the wait-on-address primitive that parks/wakes one,
`exit` ends one, `sched_yield` hands off. FP/SIMD state is saved/restored eagerly
per switch (two contexts time-share the vector registers) and the TLS base is
reloaded per context. This is the *cooperative* form: unlike a strand runtime
that yields at every await point, a Linux thread only yields when it issues a
syscall, so **a compute-bound thread that never syscalls starves its siblings**
until the preemption doorbell (section 4, task #27) - accepted and documented.
Two other section-6/1-mandated properties are deferred with the same honesty:
**priority inheritance** on futex wake is a TODO (plain FIFO for now; no
RT-reservation mutexes in the L4 suite), and multiple *vcores* (real SMP
parallelism) still awaits secondary-core bring-up. The native strand runtime
(sections 1-3) remains the single-vcore userspace scheduler.

**The first real wakeup (docs/LIBRHEO.md Phase D).** Until Phase D, "park on a
token" was closed only by a synchronous doorbell drain - a reactor with nothing
ready could only spin, because the kernel had **no interrupts on any ISA**. Phase
D adds the OS's **first block-and-wake**: a librheo `term` cell whose strand parks
on console input drives the reactor to block in `SYS_WAIT_INPUT`, and the kernel
**idles at `wfi` until the UART RX interrupt delivers a byte** (RISC-V, S-mode
external via the AIA IMSIC) instead of spinning - a genuine 0%-CPU park, one
wakeup resuming the parked strand. x86-64/ARM64 still poll (their
interrupt-controller bring-up is pending); the general completion-queue IRQ and
the preemption doorbell (section 4) remain future work.

## 2. Both stack disciplines

- **Stackless** (async state machines): bytes per strand, no stack, maximum
  depth known at compile time - for millions of I/O-bound waiters.
- **Stackful** (lazy-commit stacks, MEMORY.md 3): deep call chains, FFI,
  legacy libraries. Reserve big virtual, commit lazily, guard page always.

## 3. The common issues, each answered structurally

| Issue | Structural answer |
|---|---|
| Stack overflow | Unmapped guard region below each stackful stack; clean fault kills the strand, not the cell. Stackless strands cannot overflow. |
| Oversubscription | Gone by construction: the runtime knows exactly how many vcores it holds; no guessing against invisible neighbors. Vcore revocation arrives as an activation event and the runtime sheds deliberately. |
| A strand hogging a vcore | Compiler yield checks at loop back-edges and allocation sites (Go's approach) + a per-vcore **preemption doorbell**: a self-armed timer whose fire delivers a user-level interrupt forcing the runtime's scheduler onto that vcore. |
| Lost wakeups | Park/unpark is sequence-counted (wait-on-address with a generation): "park unless the count moved since I checked." The check-then-sleep race is eliminated at the primitive. |
| Thundering herd | Wake-one is default; wake-all is an explicit call. Directed handoff transfers ownership straight to a specific parked strand, skipping the scramble. |
| False sharing | Tier-1 ABI structs are cache-line aligned; per-vcore arenas keep two strands' allocations off one line; the topology graph lets tooling surface cross-vcore line bouncing. |
| TLS bloat | Thread-locals become **vcore-locals** (N of them, not 100k) plus explicit strand-local storage for the rare true need. |
| Data races | Three rings - see section 5. |
| Deadlocks | Within a cell, the runtime owns every park and maintains the wait-for graph, detecting cycles cheaply in debug/canary builds (report, do not guess a victim). Across cells, waits are leases with expiry, so a cycle becomes bounded lease-expiry events, never a silent hang. |
| Priority/deadline flow | Strands inherit their cell's reservation; the runtime schedules strands EDF-or-priority within budget; PI-mandatory locks extend into the strand layer. |

## 4. User-level interrupts and doorbell preemption

The preemption doorbell needs the kernel to deliver an interrupt *into
userspace* without a full kernel round trip. Hardware user-level interrupts
(x86 UINTR, ARM equivalents via the Arch trait) make this cheap; where absent,
a signal-like fallback with higher cost is used. The runtime arms its own
doorbell timer, so a runaway compute loop is preempted **by the cell's own
runtime**, no kernel scheduling policy involved. The FFI backstop: a C library
that busy-loops defeats compiler yield-point insertion, and the doorbell is
what preempts it anyway.

## 5. Data races - three rings

- **Between cells:** impossible by construction - separate address spaces,
  sealed shared buffers (IO.md 3). Sharing is explicit ownership transfer.
- **Within a Rust cell:** `Send`/`Sync` and the strand API's types make racy
  sharing a compile error; queue disciplines are encoded as typestate.
- **Within a C/legacy cell:** the OS honestly cannot save you inside your own
  address space, but it **contains** you - your races corrupt only your cell,
  and TSan-style instrumentation runs as a cell-local debug capability.

## 6. Locking

The design is deliberately opinionated.

**In the kernel - almost none.** Per-core data structures wherever possible
(the multikernel instinct: cores communicate by message, not shared
mutation); RCU-style **epoch reclamation** for read-mostly hot state
(capability lookup is a lock-free read); short non-preemptible spinlocks only
inside per-core critical sections. Queues are lock-free rings (SPSC where the
typestate allows). The kernel never sleeps holding a lock because it barely
sleeps - it is a control plane.

**For userspace, three primitives with the pathologies designed out:**

1. **Futex-equivalent** (wait-on-address via the queue mechanism), but with
   **priority inheritance mandatory** for any mutex a reservation-holding
   strand touches. Priority inversion (the Mars Pathfinder bug, still alive in
   Linux unless you opt into PI futexes) breaks RT contracts, so PI is not
   optional and the admission math assumes it.
2. **Cross-cell locks are leases, never mutexes** - revocable, expiring,
   fencing-token-protected, because a peer can crash holding one. Local and
   distributed locking are one mechanism at two scales (doctrine 9).
3. **The steering current:** the design pushes toward not sharing mutable
   state at all - queues and ownership transfer are the blessed pattern, and
   Rust's ownership makes the blessed pattern the easy one. Locks are for the
   cases where sharing genuinely wins (a database's buffer pool, inside one
   cell). The OS does not police intra-cell locking; it only guarantees a cell
   cannot export its deadlocks.

## 7. Verification

Work-stealing deques are the classic Chase-Lev structure - the one place lock-
free subtlety earns its keep, verified with **loom** permutation testing
(TOOLING.md 5). Every lock-free ring and the epoch reclaimer get the same
treatment.

## 8. Performance posture

Strand switch ~tens of ns (register save + scheduler pick, userspace);
park-and-run-next on I/O similar plus the submission write; kernel involvement
per I/O amortizes toward fractions of a syscall via batching. Hot server fast
path: doorbell rings, poller drains 64 completions, 64 strands run, one
batched submission flush.

## 9. Honest costs

- User-level scheduling makes debugging harder by default - the kernel sees N
  vcores, not 100k strands - so runtime introspection is mandatory
  (OBSERVABILITY.md 7).
- Stackful + FFI pins behaviors the runtime cannot see inside; the doorbell is
  the backstop, not a cure.
- Two stack disciplines push real complexity into language runtimes - the
  deliberate trade of the two-level design: simple kernel, sharp runtimes with
  real responsibility.
