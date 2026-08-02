# The execution model: one entity, from machine down to strand

This document designs the thing every scheduler decision in this kernel is *about*.
It is a framework design, top-down, with the hierarchy drawn, the dependency graph
drawn, the edge cases enumerated on that graph, and every use case simulated against
it. It owns docs/SUBSTRATE.md pillar 3 (vcores) and docs/SMP.md 10.2's remaining
question (threads of one cell across cores).

It exists because the incremental route has stopped paying. Read section 1 first: five
separate defects, each found by a test, each fixed narrowly, and all five are the same
defect.

---

## 1. Why this document exists: five defects, one shape

| # | Defect | Where it presented |
|---|---|---|
| 1 | `drain_cells` stamped per-vcore ownership for a whole batch *before* winning any run-mark, so a core holding two vcores could enter one a stealer had already taken | double entry, downstream corruption |
| 2 | `next_sibling_vcore` checked ownership but not *parked*, so a yield entered a sibling parked mid-syscall and resumed it at its syscall return | `SYS_ARM_TIMER returned 47, want 0` - no fault, no log |
| 3 | `nproc`'s block state was per **cell**, so one context parking recorded the wait for all of them and the scheduler idled a machine with work available | sibling ran 0 rounds |
| 4 | `linux::plock` covered the syscall dispatch but not the two trap-context entry points; they were unreachable rather than safe | nothing - until dispatch was turned on |
| 5 | `run_cells_on_both` published two cells and claimed neither, so a peer's preemption scan switched into the cell this core was running | instruction fetch at address 0, both cores |

Every one is the same sentence: **several places must agree about an execution entity,
and no single place decides.** Defect 1 is claim-vs-enter disagreeing. 2 is
owner-vs-runnable. 3 is cell-vs-context. 4 is syscall-path-vs-trap-path. 5 is
publisher-vs-runner.

Fixing each narrowly is correct and also guarantees the next one, because the generator
is untouched. The generator is that there is no single execution entity: there are three
representations of one idea, and the agreement between them is maintained by hand -
counted in this tree today, **two** ownership predicates called from **eight** sites
across two files (`nproc.rs` 5, `linux/proc.rs` 3), and **four** claim sites in
`smp.rs`, none of which is where the entity is entered.

### 1.1 The three representations, measured

```
  what it is            where it lives                     keyed by
  ------------------    -------------------------------    ---------------------
  scheduling entity     sched::vcore::Vcore                (cell, context)   <-- already unified
  native context        user::RunCell.v*[MAX_VCORES]       (cell, vcore)
  Linux context         linux::thread::THREADS[cell][i]    (cell, thread)
```

`sched::Vcore` is *already* the unified entity for the ordering decision. Its own field
comment says so: `context` is "which of the cell's contexts this is (a Linux thread, a
native vcore)". So the scheduler's *policy* half is unified and has been since S3'.

What is not unified is everything the entity needs in order to actually run:

| Resource an entity must own | Native | Linux |
|---|---|---|
| Trap frame | `RunCell.vframe[v]` | `thread::FRAMES[cell]` element `i` |
| FP/SIMD save area | `CELL_FP[cell * MAX_VCORES + v]` (static) | `thread::FPAREAS[cell]` element `i` (funded) |
| Kernel stack | in the frame, one per cell slot | **in the frame, copied from the parent by `clone_child_frame` - so every context of a cell shares one** |
| Queue pair | `RunCell.vqp[v]`, `vqp_va[v]`, `vqp_cap[v]` | none |
| Owning CPU | `RunCell.vcpu[v]` | none - the *cell* is claimed, not the context |
| Runnable / parked | `nproc::Proc.vparked[v]`, `.vblock[v]` | `Thread.state`, `Thread.pblock`, and `linux::Proc.state` |
| Entered-by guard | `user::INSIDE` (`PerCpu<vid>`) | the same, keyed on the cell's vcore 0 |

Three consequences fall straight out of that table, and they are the three things this
kernel cannot currently do:

1. **A Linux context cannot run on another core.** Not for want of a lock: it has no
   owning CPU field and no kernel stack of its own. Two contexts on two cores would trap
   onto one stack.
2. **A native vcore's FP area is a fixed static** (`MAX_CELLS * MAX_VCORES` areas in
   `.bss`), which is why `MAX_VCORES` is 4. The Linux side already funds its equivalent.
3. **Runnability has two to three authorities per personality**, reconciled at each pick.
   Defect 3 was one of those reconciliations being wrong; defect 2 was another.

---

## 2. The hierarchy

Two hierarchies, and keeping them separate is the whole design. One is **hardware**, one
is **software**, and they meet at exactly one place: an entity is claimed by a CPU.

```
HARDWARE                                   SOFTWARE
--------                                   --------
machine                                    capability bundle  (a container, a pod)
  |                                          |
  +-- NUMA node  (memory + CPU affinity)     +-- cell         (an address space, a budget,
        |                                    |     |            a PrincipalId)
        +-- CPU  (core class: P / E / LP)     |     |
              |                              |     +-- ENTITY  (a vcore: one execution
              |   claims                     |           |       context - trap frame,
              +----------------------------- + ----------+       kernel stack, FP area,
                                                         |       queue pair, owner CPU)
                                                         |
                                                         +-- strand  (userspace only -
                                                               the kernel never sees one)
```

Read the meeting point carefully, because it is the answer to "what information does the
scheduler need":

- The kernel scheduler's unit is the **entity**. It never sees a strand.
- A **strand** is a userspace future multiplexed onto an entity by `runtime/`. The
  in-cell scheduler is the second level of the two-level scheduler docs/SCHEDULING.md 3
  already specifies. The kernel exports the entity's burst score read-only so both
  levels use one metric and neither guesses.
- A **process** in the POSIX sense is a cell. A **thread** is an entity. A **task** in
  the async sense is a strand. Those are three different levels and the confusion between
  them is what the current three representations encode.

### 2.1 Where accelerators fit, and why tiles are foundational rather than a library

A GPU, an NPU or a systolic tile array is a **core class**, not a new kind of object:

```
        +-- CPU, class P    (latency-first: interactive entities, reservations)
        +-- CPU, class E    (throughput-first: batch, compaction, log shipping)
        +-- CPU, class LP   (energy-first)
        +-- engine, class GPU / NPU / accel   (attested, driven by a driver cell)
```

One placement decision, one taxonomy, one set of information. A tile program is then an
ordinary entity whose *class* asks for a long slice and node-pinned memory, and whose
lowering target is one of the engine classes. That is what makes tiles framework rather
than library: `librheo::tile` builds the program, and the *same* placement machinery that
puts a Node worker on a P-core puts a tile program on an engine. Nothing about tiles gets
its own scheduler path. When no engine exists the class resolves to CPU and the answer is
`EngineUnavailable` where it cannot - never a faked engine (docs/TILES.md).

---

## 3. The dependency graph

Two graphs. **State** dependencies say who may read or write what. **Ordering**
dependencies say what must happen before what. Every defect in section 1 is a missing
edge in one of them.

### 3.1 State dependency graph (target)

```
                        +-------------------+
                        |  ENTITY TABLE     |   one row per (cell, context)
                        |  the one authority|   owner, state, resources, sched fields
                        +-------------------+
                           ^     ^      ^
             asks (order)  |     |      |  asks (runnability)
          +----------------+     |      +------------------+
          |                      | owns                    |
  +---------------+       +--------------+          +----------------+
  | READY QUEUE   |       |  CPU (PerCpu)|          |  PERSONALITY   |
  | EEVDF + BORE  |       |  current ent.|          |  nproc / linux |
  | per CPU       |       |  timer arb.  |          |  wake sources  |
  +---------------+       +--------------+          +----------------+
                                                            |
                                                            | owns
                                                     +--------------+
                                                     | CELL STATE   |
                                                     | fds, VMAs,   |
                                                     | signals, cwd |
                                                     +--------------+
```

Three rules, and they are the whole framework:

- **R1. The entity table is the single authority on `owner` and on `runnable`.** The
  personality *supplies* wake sources and *declares* transitions; it does not keep a
  second copy of the answer. Today it keeps two or three (section 1.1), and reconciling
  them at each pick is defects 2 and 3.
- **R2. The ready queue decides order, never runnability or ownership.** This rule is
  already in force at the `sched::dispatch` seam and is the one part of the current design
  that has produced no defects. It stays exactly as it is.
- **R3. A CPU reaches an entity only through the table.** No path publishes an entity by
  another route - which is defect 5, where `run_cells_on_both` was such a route.

### 3.2 Ordering dependency graph (an entity's life)

Each arrow is a hard ordering. The label on the arrow is what breaks if the order is
wrong, and where that has already happened.

```
   create ---- fund resources ----> IDLE
                                     |
                     (a) claim(cpu)  |   must precede enter, or two cores enter
                                     v   -> defect 1, defect 5
                                  CLAIMED
                                     |
                     (b) win run-mark|   exactly one core may turn 0 -> 1
                                     v
                                  ENTERING
                                     |
                     (c) save peer FP|   before touching the register file
                                     |   -> the SYS_YIELD FP scar
                     (d) restore own |
                     (e) mark INSIDE |   per CPU, keyed on the entity
                                     v
                                  RUNNING  <----------------+
                                     |                      |
              +----------------------+------------+         | (i) re-arm slice
              |               |                   |         |     -> ARM64 took 0
     (f) syscall/trap  (g) slice fires     (h) exit         |        preemptions
              |               |                   |         |
              v               v                   v         |
           BLOCKED         PREEMPTED            EXITED       |
              |               |                   |         |
   (j) record wake source     +-------------------+---------+
       PER ENTITY, not per cell                    |
       -> defect 3                     (k) release resources,
              |                            clear owner, exactly once
   (l) wake only via a source                      |
       the table holds                             v
              |                                  IDLE
              +--> CLAIMED
```

Two edges deserve their own note because both are currently wrong:

- **Edge (i), re-arm. Fixed (stage E5), and it took two defects rather than one.** A slice
  used to be armed at first entry, at a cell-level reschedule, and by the preemption path
  itself - **not** on an ordinary syscall return. So a cell whose contexts are scheduled
  inside the cell (Node, Bun) got one slice and, if it did not fire, nothing armed another:
  ARM64 armed 2 slices for two whole programs and took 0 preemptions, where x86-64 armed
  322 and riscv64 150 for the same binary. The fix is the rule the FP swap already got -
  *every* return to user arms, at **one** site, so no path can forget - and it arms the
  slice's **remainder** rather than a fresh slice, because a full slice on every return
  would let a cell syscalling every 100 us push its deadline out forever, which is the
  starvation this prevents wearing the costume of a fix.

  Arming alone was not enough, and the second defect is the more interesting one:
  `dispatch::running` **looked the vcore up and never admitted it**. The only thing that
  admitted a vcore was `pick`'s `sync_runnable`, so a cell that never reached a cell-level
  reschedule was never in the queue at all - the running record stayed empty and the
  CPU-time charge, the burst score *and* the new re-arm all silently did nothing. It was
  invisible on two ISAs and total on the third, and it was found by a counter rather than
  by reasoning: the E5 site was reached **472** times on ARM64 and declined all 472
  (`dispatch::rearm_counters`, which exists to distinguish "never reached" from "reached
  and declined" - an unchanged `armed` count cannot). "The vcore this CPU is running is in
  the queue" is now an invariant established where the CPU starts running it.

  A third thing, which is an ordering rule rather than a defect: on ARM64 a cell's SPSR
  carries its IRQ mask and `trapframe_new` derives it from `dispatch::enabled()`, so
  enabling dispatch **after** building the frames gives a cell that runs at EL0 with IRQ
  masked - 474 slices armed, 0 interrupts taken. x86-64 and riscv64 read their mask at the
  same point, so it is one rule, not a per-ISA workaround: enable dispatch before
  `trapframe_new`.
- **Edge (a) before (b).** Claiming and winning the run-mark are two steps and the order
  matters, which defect 1 found. In the target they are **one** operation on the entity
  row - a single compare-exchange that both claims and marks - so the order cannot be got
  wrong because there is no order.

### 3.3 The lock hierarchy

A framework needs a lock *order*, stated once, or the finer locking of docs/SMP.md 10.2
deadlocks the first time two are held together.

```
   1. entity row      (per entity, or lock-free CAS on the owner word)
   2. cell state      (per cell - the Class A tables of SMP.md 10.2a)
   3. global registry (per registry - the Class B tables; today one plock)
   4. frame allocator (mm::frames, one SpinLock)
```

Acquired in increasing order, released in any order. Nothing at level N may wait on
level N-1. Two consequences worth stating:

- The demand-paging fault handler runs at level 2 and takes level 4. A syscall reaches
  the fault handler through `uaccess`, so level 2 must be **recursive per CPU** - which
  is what `plock` already is, and the reason it is.
- A syscall that idles inside the trap holds level 2 while it halts. That is latency, not
  deadlock, and it is the first thing the split removes (the wait belongs to the entity,
  so it should be released before parking).

---

## 4. The entity, and its information budget

The user requirement is precise: give the scheduler enough information to do meaningful
work, without a data structure so large that carrying it costs more than the decisions it
enables. So the layout is designed against cache lines, and each field has to justify
itself by naming a decision it makes.

### 4.1 Hot line - 64 bytes, read on every pick

Everything the ready queue touches to choose, in one line:

| Field | Bytes | The decision it makes |
|---|---|---|
| `cell: u16`, `context: u16` | 4 | identity |
| `owner: u16` | 2 | may this CPU pick it (R1) |
| `flags: u16` | 2 | runnable, parked, live, entered, wants-migrate |
| `class: u8` | 1 | reserved / fair / batch |
| `core_class: u8` | 1 | P / E / LP / engine preference |
| `node: u8` | 1 | which memory node its pages are on |
| `_pad: u8` | 1 | |
| `vdeadline: u64` | 8 | the EEVDF ordering key |
| `vruntime: u64` | 8 | eligibility |
| `slice_ns: u32` | 4 | how long before reconsidering |
| `weight: u16` | 2 | BORE burst score, as an integer log2 |
| `burst_bits: u16` | 2 | the raw burst accumulator |
| `hard_deadline_ns: u64` | 8 | EDF, reserved class only |
| `runnable_since_ns: u64` | 8 | queue delay - the responsiveness number, measured |
| `wake: u32` | 4 | which wake source it is parked on (an index, not a struct) |
| `_pad2: u32` | 4 | |

That is 64 bytes exactly: **one pick reads one line.** Note what is *not* here -
accounting totals, resource pointers, blocking detail. They are not needed to choose.

### 4.2 Cold line - read on entry and exit, not on a pick

| Field | Bytes | Note |
|---|---|---|
| `frame: *mut TrapFrame` | 8 | |
| `kstack_top: u64` | 8 | **per entity** - this is the field whose absence blocks Linux threads across cores |
| `fp: *mut FpArea` | 8 | funded, sized by CPUID, so `MAX_VCORES` stops being a number |
| `qp: *const QueuePair`, `qp_va: u64`, `qp_cap: u32` | 20 | one submission ring per entity, single-producer by construction |
| `service_ns: u64`, `dispatches: u32`, `preemptions: u32` | 16 | accounting; the fuzzer's oracle for I6 |

### 4.3 Storage, and what removes the last fixed limit

The table is a `Funded<Entity>` charged to the owning cell (docs/SUBSTRATE.md pillar 1,
the mechanism S1' already applied to the Linux context tables). Consequences:

- `MAX_VCORES = 4` disappears. It is 4 today only because the FP areas are a fixed
  `.bss` array of `MAX_CELLS * MAX_VCORES`.
- `MAX_THREADS` is already gone on the Linux side; this makes the two the same table.
- Exhaustion is a per-cell `-EAGAIN` or `-ENOMEM` naming the cell, never a global
  "table full" (the no-OOM-killer doctrine, docs/MEMORY.md 7).

### 4.4 Defaults - what a cell that asks for nothing gets

A framework is judged by its defaults. These are chosen so that a cell written before
any of this existed behaves **identically**, which is also the additivity rule
(docs/ENGINEERING.md).

| Property | Default | Why this one |
|---|---|---|
| Entity count | 1 | one context is the overwhelming case; asking for more is a launcher verb |
| Class | `Fair` | a reservation is something you must ask for and be admitted for |
| `slice_ns` | 1 ms | `DEFAULT_SLICE_NS` today; long enough that a syscall-heavy cell is not preempted mid-burst, short enough for interactive response |
| Weight | base | no burst credit until behaviour is observed - the score is measured, never assumed |
| `core_class` | none (any CPU) | a preference nobody expressed must not become a restriction |
| `node` | the cell's home node, round-robin across nodes at creation | spreads bandwidth instead of piling on node 0 (docs/SUBSTRATE.md pillar 6) |
| Owner | unclaimed | an unclaimed entity is pickable by every core, which is exactly single-CPU behaviour |
| Kernel stack | one page from the cell's budget, per entity | the smallest thing that is *correct*; sharing is what breaks |
| FP area | sized by CPUID at boot, ABI-default contents | a fresh entity must have a well-formed MXCSR |
| Queue pair | inherited region, own ring | `SYS_QUEUE_INFO` answers per entity, so a binary need not know entities exist |

The two defaults that are choices rather than obvious: **unclaimed** (the alternative,
claiming at creation, would pin a forked child to its parent's core forever - and pinning
it *for its first run* is right, which is what "inherit the parent's owner" already does)
and **1 ms** (a number, and the honest position is that the right value is a measurement
this container cannot take - it is a default, tunable per class, not a constant of nature).

---

## 5. Invariants - the oracle

These are what the fuzzer checks and what every review checks. They are stated as
properties of the entity table, so each has exactly one place to hold.

| # | Invariant | The defect it would have caught |
|---|---|---|
| I1 | No entity is entered by two CPUs at once | 1, 5 |
| I2 | `owner` is `NO_CPU` or an online CPU | - |
| I3 | An entity is pickable by a CPU if and only if it is live, runnable, and owned by that CPU or unclaimed | 2 |
| I4 | A parked entity holds at least one wake source, or the machine reports deadlock | (the pre-2.4 panic) |
| I5 | Work conservation: no CPU idles while a runnable entity it may pick exists | 3 |
| I6 | `sum(service_ns)` equals the sum of charged slices, and every stop is recorded as voluntary or involuntary exactly once | - |
| I7 | Resources are released exactly once; the table returns to its starting size | (the S1' slot leaks) |
| I8 | A blocked entity's block is per entity: a cell is blocked only when every live entity of it is | 3 |
| I9 | The last entity out ends the cell; an exited entity is neither runnable nor parked | (the vcore-exit rule) |
| I10 | Every return to user mode arms a slice, or reports why it could not | edge (i), the ARM64 zero-preemption case |

I5 and I10 are the two that no current test asserts and that a fuzzer can. I5 in
particular is the invariant a scheduler is *for*, and it has never been checked here.

---

## 6. Simulation: use cases against the graph

Each row walks a real workload through section 3.2's edges. `x` means the use case
traverses that edge. The value is the **columns with few marks** - an edge nothing
traverses is an untested edge, and section 1 is a list of untested edges failing.

| Use case | a claim | b mark | c/d FP | f block | g slice | i re-arm | j per-entity wake | k release | Notes |
|---|---|---|---|---|---|---|---|---|---|
| One native cell, one entity (every pre-vcore kernel) | x | x | x | x | | x | | x | the baseline that must not change |
| Two native cells, two cores | x | x | x | x | | x | | x | proven |
| One cell, two entities, two cores | x | x | x | | | x | | x | proven; FP/kstack sharing **undetectable** here |
| One cell, two entities, one core (yield) | x | x | x | x | | x | x | x | proven |
| Entity blocks, sibling runs | x | x | x | x | | x | x | x | proven (`schedidle`) |
| Native cell preempted | x | x | x | | x | x | | x | proven (`preempt`) |
| Linux cell, one context | x | x | x | x | x | x | x | x | proven |
| Linux cell, 4 contexts, one core | x | x | x | x | x | x | x | x | proven |
| **Linux cell, 4 contexts, several cores** | | | | | | | | | **blocked: no per-entity kstack or owner** |
| Linux cell forks across cores | x | x | x | x | | x | x | x | proven; child inherits owner |
| Linux signal delivery under preemption | x | x | x | | x | x | | x | FP across a handler proven; **signal to an entity on another core is not** |
| Node.js (V8 + libuv, epoll + eventfd) | x | x | x | x | x | partial | x | x | edge (i): re-arm only via reschedule |
| Bun (JSC + clone3 worker) | x | x | x | x | x | partial | x | x | as Node; the worker is cooperative within a core |
| Claude Code (Bun-compiled, 275 MB) | x | x | x | x | x | partial | x | x | as Bun |
| Tile GEMM across cells | x | x | x | | | x | | x | data-parallel over cells, not entities |
| FlashAttention 2 across cells | x | x | x | | | x | | x | proven bit-identical |
| **FlashAttention 3 producer/consumer overlap** | | | | | | | | | **blocked: needs two entities inside one slice** |
| Kafka-shaped append log | x | x | x | x | | x | x | x | needs per-entity queue (have) + zero-copy (have) |
| TLS server, N clients across entities | x | x | x | x | x | x | x | x | N4a fan-out is one entity today |
| Container / pod (bundle of cells) | x | x | x | x | x | x | x | x | budgets are per bundle; no new mechanism |
| Driver cell (FUSE / LKL, DRIVERS.md) | x | x | x | x | x | x | x | x | wants funded tables (have) |
| A CPU runs dry and steals | x | x | | | | | | | proven for **unstarted** entities only |
| **Migrating a running entity** | | | | | | | | | **attempted twice, reverted twice** |

### 6.0a The simulation is executable

Nine of those rows are now **run**, not tabulated: `verify/entity/`'s scenario suite
drives each shape as a deterministic sequence against a hand-computed expectation, and
each has a control observed failing (`verify/README.md` has the table). Two carry the same
note - "threads of one cell across 4 cores" and "FA3 producer/consumer overlap" have no
control, because they assert a capability the **model** permits and the **kernel** does
not yet hold the field for (stage E4). There is nothing to break in the model to make them
fail, which is exactly why they are written now: when E4 lands, these are the tests it has
to satisfy, and they exist before the implementation rather than after it.

The one that is worth reading is the Node-teardown scenario. It asserts that a cell with
one parked entity and one runnable entity is **not** blocked, and its control - making
`all_parked` treat any parked entity as blocked - reproduces **defect 3 by name**, in
milliseconds, on the host. That defect originally cost an in-QEMU four-core boot to find.

### 6.1 What the simulation found

Four gaps, and each is now a *named consequence of a missing field* rather than a
mystery:

1. **Linux threads across cores** needs `kstack_top` and `owner` per entity. Both are in
   section 4's cold line. Nothing else about it is hard - the lock hierarchy of 3.3
   already says which lock protects what.
2. **FA3's real overlap** needs two entities inside one cell's slice - which is exactly
   what a per-entity slice (edge i, invariant I10) gives. The tile framework needs no
   change; the entity model does.
3. **Cross-core signal delivery** is untested because a signal to an entity another core
   is running has no delivery path. In the target it is one: mark the entity's pending
   set, and either it is not running (delivered when entered) or an IPI makes its core
   take the trap-exit path. The IPI is the only genuinely new mechanism this document
   proposes, and it is mechanism, not an object.
4. **Migrating a running entity** stays out. It failed twice, and the fourth finding
   recorded in docs/SMP.md 10.0 is why: with resources per *entity* and a claim that is
   one compare-exchange, migration becomes "change `owner` while not `RUNNING`" - which
   is a state-machine edge rather than a race to reason about. It is still not built, and
   the honest position is that the model makes it expressible, not that it is done.

### 6.2 Edge cases enumerated on the graph

The ones a use-case table will not surface, each with the invariant that catches it:

| Edge case | Catches |
|---|---|
| Entity exits while a peer holds a reference into its row | I7 |
| Last entity exits while a sibling is parked with a live wake source | I9, I4 |
| Entity created (`fork`, `clone`) between a peer's pick and its enter | I1 |
| Owner core goes offline holding a claim | I2 |
| Wake arrives for an entity that has already exited | I4 |
| Two wake sources satisfy at the same instant | I4 |
| Slice fires while the kernel holds a reference into a funded table | I1 - why "note it" and "act on it" are split |
| Slice fires during entity creation | I10 |
| Steal races the owner's own first entry | I1 |
| Cell budget exhausted mid-`clone` | I7 - rollback |
| Reservation admitted, then its entity exits | I6 |
| All entities parked, none satisfiable | I4 - deadlock, reported not panicked |
| An entity parked on a source only its own core can service | I5 |
| FP area smaller than the running CPU's state (heterogeneous cores) | 4.2 - size by the *widest* CPU |
| Entity's node differs from its owner's node after a steal | I5 over locality: work conservation wins, and the crossing is counted |

That last row is a policy statement worth making explicit: **when locality and work
conservation conflict, work conservation wins and the crossing is counted.** The
alternative is a core idling beside runnable work, which is a worse failure and a silent
one.

---

## 7. Foundational CPU support: FRED, and the backwards-compatible path

Intel FRED (Flexible Return and Event Delivery) is the right foundation for the x86-64
event path, and the reason is not novelty - it is that FRED deletes an entire defect
class this tree has hit **four separate times**.

### 7.1 The scar FRED removes

The SYSRET-provenance defect, found four times in this tree:

1. the first ring-3 fault resume,
2. `rt_sigreturn` rewriting its frame in place,
3. `enter_user_first` re-entering a timer-captured frame on a secondary,
4. the fault resume that genuinely re-executes.

Every instance is the same fact: `SYSRET` **consumes RCX and R11**, so it is correct only
when returning from the syscall that was entered by `SYSCALL`. Getting it wrong is not a
fault - it is two corrupted registers and a program that stops making sense. The rule was
eventually written down ("SYSRET is only for returning from the syscall it was entered
by") and the resume paths moved to `IRET`.

Under FRED there is one return instruction pair, `ERETS` (to supervisor) and `ERETU` (to
user), and **neither consumes a general-purpose register**. The defect class cannot be
expressed. That is worth more than any performance number attached to FRED.

### 7.2 What else it gives this design specifically

| FRED property | What it fixes here |
|---|---|
| Event delivery always to a defined **stack level** (4 levels, chosen per event type) | per-entity kernel stacks plus nested-event safety become structural rather than an IST table hand-maintained per vector |
| Kernel `GS` loaded by the CPU on entry | `swapgs` disappears. This tree deliberately never gives a cell a GS base *because* `swapgs` is error-prone; FRED removes the reason for the restriction |
| `CS`/`SS` always valid on entry | the ring-3 frame capture in `vectors.S` stops being special-cased |
| Nested-event state in the event frame | a slice firing while the kernel holds a funded-table reference is a defined state, not a hazard to split "note" from "act" around |
| One entry point for all events | the trap path stops being "syscall stub plus N vector stubs", which is where three of the four SYSRET instances lived |

### 7.3 The compatibility rule - already this tree's law

FRED is enabled **by observation, never by assumption**, exactly as the LAPIC access mode
and the three wake modes are (docs/SMP.md 5, docs/NETSTACK.md 16):

```
   probe CPUID.(EAX=7,ECX=1):EAX[17] (FRED) and [18] (LKGS)
      |
      +-- present -> set CR4.FRED, publish the RSP/SSP stack-level MSRs per core,
      |              install the single event entry point, verify by taking one
      |              synthetic event and reading back where it landed
      |              -> arch::event_mode() reports Fred
      |
      +-- absent  -> the IDT path exactly as today, byte for byte
                     -> arch::event_mode() reports Idt
```

Three requirements on that, from this tree's own scars:

- **Verify, do not claim.** A claimed event path that never delivers is a hang, not a
  slow path - the lesson the NVMe interrupt path recorded. So bring-up takes one event
  through the new path and checks where it landed before anything depends on it.
- **Report the mode.** `event_mode()` is printed at boot and readable by tests, so a
  proof never says "FRED" about an IDT run.
- **Both paths stay proven.** QEMU support for FRED is recent and this container's QEMU
  8.2 almost certainly lacks it, so the IDT path is not legacy - it is the path the whole
  test suite runs on, and the FRED path is the one that will skip-with-reason here and be
  gated at the lab. That is the honest shape and it is the same one the tree uses for
  AVX-512, PMEM on arm/riscv, and the RISC-V IOMMU.

ARM64 and RISC-V need nothing equivalent: `eret` and `sret` consume no registers and both
ISAs already carry the interrupted state in a frame. FRED therefore *reduces* per-ISA
divergence - it makes x86-64 look like the other two, which is docs/TARGET-ARCHITECTURES.md
4's goal rather than an exception to it.

---

## 8. The fuzzer

The scheduler and the entity state machine are integer-only, allocation-free and
portable, and there is already a precedent for running kernel code on the host unedited:
`comparison/linux/rheo_sched.rs` runs the shipped `sched/{bore,vcore}.rs` over a scripted
trace. So the state machine can be **model-checked on the host**, fast, with a
deterministic seed.

```
  seeded RNG -> random operation sequence over N entities, M CPUs:
      create / claim / enter / syscall / block / wake / slice / exit / steal / migrate
                                |
                                v
                  the SHIPPED entity table + ready queue
                                |
                                v
                  assert I1..I10 after every operation
                                |
                                v
                  shrink a failing sequence to its minimum
```

**Built, and it works** (`verify/entity/`, `cargo xtask verify`): 20,000 sequences of 400
operations over 24 entities and 4 CPUs, checking I1, I2, I3, I4, I5, I7 and I9 after
every step - so 8 million operations, in about a second, with no QEMU. Every one of the
seven has a control that was observed **firing** when exactly one check was removed from
the shipped module; `verify/README.md` carries the table with seeds and operation
indices. Three results are worth more than the pass:

- **`steal` ignoring `inside` is "migrate a running entity"** - the capability attempted
  twice on real hardware in this branch and reverted twice, a full experiment each time.
  The fuzzer names it (I3) in 213 operations on the first seed.
- **`enter`'s occupied check and owner check are individually redundant and jointly
  load-bearing.** Removing either alone still passes; removing both fails at op 35. That
  is recorded rather than tidied away, because "remove one, the other covers it" is
  precisely the reasoning that produced defect 1.
- **The first `check_i5` was wrong in the way this document is about.** It asked
  `pickable` whether work existed - the code under test - so a `pickable` made too strict
  left CPUs idle beside runnable entities and the check still passed, both sides agreeing
  on a wrong answer. The oracle now computes availability from the entity's own fields.
  A passing test whose oracle is the implementation is worse than no test.

Design notes that make it worth building rather than decorative:

- **The operations are the graph's edges**, so coverage is measurable: report which of
  section 3.2's edges the run traversed. A fuzzer that never generated a steal-into-first-entry
  race has not tested I1.
- **Failures must shrink.** A 4000-operation counterexample teaches nothing; the minimal
  one is a test case.
- **It cannot replace in-QEMU proofs** and the doc says so: it checks the state machine,
  not the trap path, the page tables, or the FP register file. Those need real cores. It
  is the layer that catches the section-1 defect class *before* it needs four cores and a
  120-second boot to find.
- Host-only, dev-dependency only (`arbitrary`/`proptest`-shaped or hand-rolled - Tier
  dev per docs/SUBSTRATE.md 11), so nothing enters the kernel's dependency set.

---

## 9. Migration

Staged, additive, each stage keeping the whole suite green and the single-CPU path
unchanged. The order is forced by the dependency graph, not chosen.

| Stage | Change | Proof | Unblocks |
|---|---|---|---|
| E1 | Introduce the entity table beside the current three representations; `sched::Vcore` grows the hot line's fields. Nothing reads it yet. | **Done.** `kernel/src/sched/entity.rs`; the suite unchanged because nothing reads it | E6 |
| E6' | The host fuzzer, brought forward - it belongs *before* the hot paths depend on the table, not after | **Done.** `verify/entity/`, `cargo xtask verify`: 8M operations, 7 invariants, 7 firing controls | E2-E5 can be refactors with an oracle already in place |
| E2 | Move `owner` and the entered-guard into the table; the claim and the run-mark become one compare-exchange. Delete the two predicates, leaving one. | I1, I3; the 8 predicate sites become 1, and the 4 claim sites become 0 - the claim happens where the entity is entered | removes defects 1, 2, 5 by construction |
| E3 | Move `runnable`/`parked` into the table; the personality declares transitions and stops keeping copies. | I4, asserted in a real boot for the first time - "parked with no wake source" is a state a personality-side boolean cannot express | removes defect 3's class |
| E4 | Per-entity resources: funded FP area. `MAX_VCORES` stops being the resource limit. | frames measured per context, and returned with the slot | Linux threads across cores; FA3 overlap |
| E5 | Arm the slice at the single return-to-user site. | **Done.** `on_user_trap` is a wrapper over `on_user_trap_inner`'s eight return paths; `dispatch::rearm_remaining` arms the slice's **remainder**. ARM64 went from 2 armed slices and 0 timer interrupts to 819 and 156, and now takes 55 preemptions where it took none | preemption independent of workload |
| E6 | Extend the fuzzer as E2-E5 land, adding I6, I8 and I10 (each needs state E1 does not hold yet). | I1..I10, with edge coverage asserted | the defect class, caught early |
| E7 | Cross-entity signal + wake IPI. | a signal to an entity another core is running | cross-core signals, migration expressible |
| E8 | FRED behind observation, IDT unchanged. | `event_mode()` reported; lab-gated | the SYSRET class deleted |

E2 and E3 are the ones that pay for the document: they turn nine hand-maintained
agreements into one, and they are pure refactors with the existing suite as the
regression gate.

### 9.1 What E2-E4 landed, and the one decision they surfaced

E2, E3's native half and E4's binding constraint are **done**; the register carries the
per-row detail. What is worth recording here is a design question none of the three could be
finished without answering, because it was invisible until the table had real users.

**The entity id is derived, and a derived id is a stride.** `entity_of(cell, vcore)` is
`cell * MAX_VCORES + vcore`, which is what removes the mapping E2 exists to delete - there is
no second identity to drift. But a stride *bounds contexts per cell*, and the two personalities
disagree about that bound by two orders of magnitude:

| Side | Contexts per cell | Bounded by |
|---|---|---|
| Native vcores | `MAX_VCORES` = 16 | the constant, now an array bound rather than a resource limit |
| Linux threads | `CONTEXT_CEILING` = 1024 | the cell's frame budget; the ceiling is only a runaway-`clone` backstop |

So the Linux half of E3 cannot simply join: its contexts do not fit the stride.

**The obvious fix is measured and refused.** Raising the stride to 1024 makes the id space
`MAX_CELLS * 1024` = 16384 entities, and `Funded::reserve` is **dense** - it allocates every
page up to the highest index touched, not the pages actually used. At 64 bytes per `Entity`
that is 256 frames, **1 MiB of funded kernel metadata**, allocated the moment the last cell
installs, for a table that will hold a few dozen live entities. That is a static array in
disguise, which is precisely what E4 removed from the FP areas; adopting it here would undo the
stage that just landed.

**The decision, for whoever lands the Linux half:** keep the derived id for native vcores, and
give a Linux thread an id **allocated** by `EntityTable::create` and stored on its `Thread`.
Two ways of *obtaining* an id, one table and one authority - which is the distinction E2 was
actually about. A stored id is not a second copy of a fact; it is a handle, the way a page-table
base is. One implementation note that is a correctness requirement rather than a preference:
allocation must start **above** the derived band (`MAX_CELLS * MAX_VCORES`), or `create` will
hand out an id inside some native cell's reserved range and a later `create_at` will overwrite
it.

This was written down rather than forced, because the Linux personality is the path Node, Bun
and Claude Code run on, and a half-migrated state machine there is the one place in this tree
where "it compiles and the suite is green" would not be evidence of much.

---

## 10. What does not change

Stated because a document this size invites the reading that everything is being
reopened:

- The capability model, the eight-object list, and the admission rule
  (docs/ARCHITECTURE.md 6). **An entity is not a new object** - it is an execution context
  of the Cell object, which is what a vcore already is. `install_vcore` stays a launcher
  verb.
- The queue ABI wire format (`abi/`).
- The kernel stays soft-float, for the reason docs/SUBSTRATE.md pillar 4 gives: no
  syscall, trap or interrupt then saves the vector file.
- W^X per mapping, no OOM killer, no ambient authority.
- `sched::dispatch`'s seam: the queue decides order, the personality decides runnability.
  R2 above is that rule, and it is the part of the current design that has produced no
  defects.
- The strand model in userspace. Strands are not entities and the kernel does not see
  them.

## 11. Honesty

- Nothing in this document is built. It is a design with the evidence for why, and the
  five defects in section 1 are the evidence.
- The section 6 table's `x` marks describe **today's** coverage, taken from the tests that
  exist. The four rows in bold are blocked, and each names the missing field rather than a
  guess.
- **No performance claim is made.** Every number here is a count (armed slices, defects,
  bytes of a struct), not a rate. The comparison with tuned Linux stays where
  docs/SUBSTRATE.md leaves it: designed to, unmeasured. QEMU under TCG models no caches
  and no TLB, so the 64-byte hot line is a design discipline justified by first principles,
  not a measured win - and it must be measured on hardware before it is claimed.
- FRED cannot be tested in this container (QEMU 8.2). Its section is a design with an
  observation-gated bring-up rule, and it will skip-with-reason here exactly as AVX-512
  and the RISC-V IOMMU do.
