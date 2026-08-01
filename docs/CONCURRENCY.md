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
(section 2), priority-inheritance locks, and vcore-local storage. The
multi-vcore build-out (per-core scheduler, work stealing, Linux threads as
vcores) is owned by docs/SUBSTRATE.md pillar 3 / stage S3, and its **execution
entity** - the one thing a strand runs on, and the boundary the kernel scheduler
never crosses - is designed top-down in docs/EXECUTION-MODEL.md. The division of
labour stated once: the kernel schedules **entities** and never sees a strand; the
`runtime/` executor schedules strands onto whichever entities the cell holds; the
entity's burst score is exported read-only so both levels use one metric instead of
each guessing at the other's.

**The kernel half of "a cell holds N vcores" is now built** (docs/SMP.md 10.0a): a
cell carries a trap frame, an FP/SIMD save area and an ownership claim **per vcore**
rather than per cell, and two vcores of one cell are proven to run on two cores at the
same instant, in one address space, on all three ISAs. `SYS_YIELD` reaches a **sibling vcore** of the same
cell before it considers another cell, and that switch changes only the FP/SIMD register
file and the frame - one address space, so no `activate()` and no TLB consequence - which
is the two-level scheduler's lower rung finally costing what section 3 says it should.
And a vcore **blocks**: the block state is per vcore, so a
context parking on a timer leaves its siblings runnable - proven by the `schedidle` oracle
one level down (`bSSSSSSSSB`, one cell, two contexts). So `TicketLock`'s "future multi-vcore
case" has a mechanism under it now. One prerequisite of the runtime half is done: the
`runtime::Heap` **global allocator is multi-core safe** (`TicketLock`, unconditionally - its
old `unsafe impl Sync` was justified by "single-CPU kernel", which stopped being true), proven
by two cores running 512 allocate/stamp/verify/free cycles each with zero cross-marker bytes
and a general protection fault when the lock is removed. **And the runtime half is built**: `strand.rs` now
holds **one executor per vcore** plus a shared injector, so strands run across vcores.

The obstacle was an API question rather than a port, and the answer is the split a `Send`
bound forces:

- **`spawn` stays vcore-local** - same signature, same `Rc` `JoinHandle`, sound because a
  strand spawned on a vcore stays on it. Every existing caller is unchanged.
- **`spawn_shared` takes a `Send` future and returns no handle.** Both restrictions are one
  fact: work that may cross cores cannot carry an `Rc`, so it cannot carry this runtime's join
  handle either. A caller wanting a result uses a channel or an atomic - which is what a
  work-stealing pool does anyway.

Each vcore's executor is `EXECS[v]`, safe by **partitioning** rather than by a lock: a vcore
belongs to one core at a time (the kernel's claim, docs/SMP.md 10.0a), so two cores here are two
disjoint elements of the array - the same argument `PerCpu` rests on. The runtime cannot know
which vcore it is on, having no register to read, so the embedder supplies an accessor once
(`set_vcore_hook`); unset it is a constant 0 and every pre-vcore caller resolves to slot 0
exactly as before. The injector is a `TicketLock<VecDeque<_>>` rather than per-vcore deques with
stealing, and the doc says why: with a handful of vcores it is not the contended structure a
many-core stealing deque solves, and whether it becomes one is a measurement there is no
hardware here to take.

Proven by the `smp` kernel on all three ISAs, in two sub-phases because they prove different
things and only one is deterministic. **Concurrent**: both cores drain the injector at once,
every one of 64 strands asserted to run *exactly* once (so a strand delivered twice fails), and
the two take counts asserted to sum to exactly 64 - which is what says every strand came off the
shared injector rather than a local queue. The split itself is *reported*, never asserted
(observed 22/42, 37/27, 35/29): one core draining all of them first is a legal schedule, and an
assertion that can fail on a legal schedule is not a proof. **Directed**: the primary spawns and
does not drain, only the secondary runs, and the secondary is asserted to have taken *all* 64
with the primary taking none - the crossing itself, deterministically. Reverting `run` so it
never takes from the injector leaves every strand unrun; collapsing the per-vcore executors to
one **hangs**, two cores corrupting one run queue.

**A cell can now ask which vcore it is**: `SYS_VCORE_INFO` reports `(index, count)`, and
`librheo::sys::vcore_index` is shaped as `fn() -> usize` so it hands straight to
`set_vcore_hook` - the runtime is *told* its index rather than inventing one, and that is the
telling. The verb's admission audit is written out at its definition in `abi/`: it adds no
object (a vcore is an execution context of the Cell object, so this is a verb over object 1
exactly as `SYS_QUEUE_INFO` is over the QueuePair), it cannot be a library (a cell has nothing
to compute its own index from - there is no register that says "you are context 1 of your
cell"), and it is two integers with all policy outside. Proven by the `smp` kernel on all three
ISAs: the **same binary** in two contexts of one cell gets indices 0 and 1 and a count of 2,
where a per-cell reply would give both the same and a hardcoded one would give both 0 -
reverting the index to a constant fails by name.

**And a loaded cell runs it.** `librheo-vcore` is the assembly: one ELF, two contexts of one
address space, each with its own ring (`load::map_queue_for`) and its own user stack
(`load::map_vcore_stack`, below vcore 0's with a one-page guard gap). Both enter at the **same
ELF entry point** - the loader resolves no symbols, and asking a cell to have two entry points
would make it read a symbol table - so librheo's crt0 branches on `sys::vcore_index()`: the
secondary skips one-time process setup, because re-running `init_heap` would reset the
allocator's free list under a sibling already using it and re-seeding the DRBG would hand two
contexts the same stream. The cell is not *told* its role by its launcher; it asks.

Its claim is the deterministic one: vcore 0 fills the injector and **never drains it**, so every
strand that ran was executed by a context that did not create it. The cell checks that itself -
all 32 strands exactly once, `shared_taken(0) == 0`, `shared_taken(1) == 32` - and only then
ends the cell `0x42`, with every other exit code (31..38) naming which check failed. Proven on
all three ISAs.

Two findings from building it. A secondary can be entered **before the primary has executed one
instruction**, because placement publishes every vcore as runnable at once and whichever core
claims first enters first - the first version assumed the primary went first and the cell never
finished, so crt0 now has a `PRIMARY_READY` flag the secondary *yields* on (bounded, so a
primary that never comes up ends that context with a distinct code rather than hanging). And a
secondary returning from `main` must exit with `SYS_EXIT` (`sys::exit_vcore`) rather than
`sys::exit`, which is `SYS_EXIT_GROUP` and would take its siblings down mid-work.

**Honest about what this phase proves and does not.** Load-bearing and observed failing: the
per-vcore user stack (sharing one wedges both contexts, exit code 38 twice) and the crt0
secondary path (without `PRIMARY_READY` the run does not complete at all). *Not* proven here:
the per-vcore **ring** - this cell's strands touch only atomics and ring no doorbell, so
collapsing both rings to one VA still passes; the ring is proven by the `smp` kernel's own
per-vcore-queue phase instead. Nor the `exit_vcore` split, whose effect here is
race-dependent - it is correct by the same argument the kernel's rule rests on, and this phase
is not its proof.

Still not built: `!Send` work that migrates (it cannot, by construction), per-vcore stealing
deques, and a cell that asks for its own vcores - the launcher still installs them, which is the
same launcher-mints-authority shape as the queue pair and the W^X exception, and a cell-facing
`spawn_vcore` is a separate design question.

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

  **That last claim is now enforced, and was not before**
  (docs/ARCHITECTURE-DEBT.md 2.4). Three kernel verbs contradicted it -
  `SYS_ARM_TIMER`, `SYS_WAIT_INPUT`, `SYS_WAIT_NET` waited *in kernel context*
  without rescheduling, so a cell's `sleep` was precisely the hidden blocking
  syscall eating the core. Each now registers its condition and returns to the
  scheduler; a **scheduler idle state** (`kernel/src/idle.rs`) halts the CPU only
  when no cell is runnable. What remains cooperative is stated plainly: a cell
  yields at a syscall boundary, so a compute-bound cell that never traps starves
  its siblings until the preemption doorbell (section 4, task #27). No wait
  consumes the CPU; not every wait is preemptible.
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

**Native cells get the same guarantee, per cell rather than per context.** A
native cell is single-context, and cells build **hard-float** while the kernel
stays soft-float (docs/TILES.md 4), so at a cross-cell hand-off the physical
vector register file still holds the outgoing cell's values. `user::switch_native_cell`
is the one native switch and swaps that register file along with the address
space - `SYS_SWITCH`, the `nproc` scheduler (`SYS_WAIT` / child exit) and the
round-robin `SYS_YIELD` all route through it, so a strand that yields the vcore
across a cell boundary keeps its FP state. The Linux path above keeps its own
per-*context* swap, because one Linux cell time-shares the registers between up
to 8 contexts. Both mechanisms and the proof are in docs/LIBRHEO.md ("FP/SIMD
across the native cross-cell switch").

**The first real wakeups (docs/LIBRHEO.md Phase D/F).** Until Phase D, "park on a
token" was closed only by a synchronous doorbell drain - a reactor with nothing
ready could only spin, because the kernel had **no interrupts on any ISA**. Phase
D adds the OS's **first block-and-wake**: a librheo `term` cell whose strand parks
on console input drives the reactor to block in `SYS_WAIT_INPUT`, and the kernel
**idles until the UART RX interrupt delivers a byte** (RISC-V S-mode external via
the AIA IMSIC; ARM64 PL011 SPI via the GICv3; x86-64 ISA IRQ 4 via the IO-APIC,
added in docs/SMP.md 8 once a working LAPIC EOI existed) instead of spinning - a
genuine 0%-CPU park on all three ISAs. Phase F adds the **second interrupt**, the **timer**: a strand
parking on a deadline (`time::sleep`/`SYS_ARM_TIMER`) idles until the per-ISA timer
interrupt fires - **interrupt-driven on all three ISAs**: RISC-V Sstc `stimecmp`,
ARM64 CNTV via the GICv3, and (since docs/SMP.md phase 1) the x86-64 LAPIC one-shot
driven over the **xAPIC MMIO** page. x86-64 had claimed this over the x2APIC MSR
block, which QEMU TCG leaves inert - rheo-net N2h made bring-up *verify* it, the claim
did not hold, and phase 1 fixed the capability rather than the wording
(docs/ENGINEERING.md 1). The general completion-queue IRQ and the preemption
doorbell (section 4) remain future work.

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
