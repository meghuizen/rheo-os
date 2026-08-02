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

E2, E3 (**both halves** - see 9.2 for the Linux one) and E4's binding constraint are **done**;
the register carries the per-row detail. What is worth recording here is a design question
none of the three could be finished without answering, because it was invisible until the
table had real users.

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

### 9.2 E3's Linux half, landed as decided above

`Thread::state` is **gone**. A Linux context is `Ready` or `Blocked` because its entity says
so - the same authority the native vcores use - and `TState` survives only as a *view*
computed from the entity, never stored. What stays on the `Thread` is the **reason**: `pblock`
(which proc-level source it is waiting on) and `fut_addr` (which futex word), because a wake
source's detail belongs to whoever owns the source, while *whether it is waiting* is the
scheduler's fact.

The id follows 9.1's decision exactly: `Thread.entity: u32`, allocated by
`EntityTable::create` above the derived band, `0` meaning "this slot holds no context". Zero
is the free marker rather than a sentinel because a funded table grows into **zeroed** frames,
so an empty value must *be* the all-zero pattern (the `mm::kmeta` contract, and the same rule
that shaped `Entity::EMPTY`). The floor is enforced in the table, not by the caller:
`EntityTable::init` takes a `reserved` count, `create` searches and grows only above it, so a
Linux id can never land inside a native cell's `create_at` range where the overwrite would be
silent.

Two paths had to become release paths, which is the S1' lesson one level along - a funded
resource handed back needs somewhere that hands it back:

- `release_cell` (a `wait4` reaping a zombie, or an unfundable `fork`) detaches every
  context's entity before releasing the tables.
- `reset` does the same for every cell between runs.

`clone` gained the matching failure path: `attach_entity` before the child slot is written,
and `-EAGAIN` if the table cannot fund one - a thread that cannot be scheduled must not be
created.

**Proven** by `linuxthreads` on all three ISAs, after four threads, twelve threads, a
rayon-threaded `sort` and two condvar timeouts have all created, parked, woken and exited
contexts: the table holds every invariant it can check, no entity records a CPU inside it, and
the teardown hands back every live context. The count is taken **across** the teardown rather
than after it, because the harness resets at the *start* of a run - after the last run the
final cell's contexts are still live, so an "all free" assertion would have passed while
proving nothing. Control observed firing: removing `detach_all` from `reset` leaves the
context live (`1 of 1 Linux context entities survived the teardown`).

Two honest limits. **I4 is checked but vacuous in that kernel** - nothing is left parked by
the time the phase runs, so the invariant's real exercise is `verify/entity`, which drives
park and wake directly; what this kernel does show about the wake source is behavioural,
since forcing every park to `NO_WAKE` makes `condwait` fail with glibc's "the futex facility
returned an unexpected error code" rather than hang. And `linux::Proc::state` - the *cell*
level Linux state, beside the per-context one this stage moved - is still its own copy; E3 is
done for contexts, not for the cell above them.

### 9.3 E4's remainder: five arrays are one record

E4 funded the FP save areas and left the rest of a context's storage as five parallel
`[_; MAX_VCORES]` arrays in `RunCell`. That was named as a remainder rather than finished,
and the number that decides whether it is worth finishing is a measurement: **704 of
`RunCell`'s 840 bytes were those arrays**, and `nproc::Proc`'s two more were 640 of its 656,
so the per-cell scaffolding was **21,504 bytes of `.bss`** at `MAX_VCORES = 16` - paid for
every slot whether or not a second context ever existed.

Five arrays indexed by the same number are one record. Saying so is what lets the *tail* of
them be funded per cell rather than reserved for all of them.

**Two designs were simulated before anything was written, and the first was wrong.** The
obvious one - "one `Funded<Vcore>` per cell", which is what S1' did for every other table -
costs a directory frame plus a data frame per table: two tables times two frames times
`MAX_CELLS` is **256 KiB of frames to save 21 KiB of `.bss`**. A `Vcore` is 48 bytes and a
frame is 4096, so funding a whole cell's contexts is 80x overhead. The FP areas were worth
funding because each one *is* a frame; these are not.

The design that works keeps **vcore 0 inline and funds only the tail**:

- every cell has exactly one context and almost every cell has only one, so the common case
  allocates nothing at all and vcore 0 stays a direct field read;
- a cell that asks for more pays one directory frame plus one data page for ~85 further
  contexts, charged to its own budget - which is what makes the ceiling the budget.

The funded tail lives in its own static (`CELL_VCORES`) rather than as a field of `RunCell`,
and that is not a style choice: `RunCell` is `Copy`, and a `Funded` descriptor must never be
raw-copied - two owners of one directory frame is the S1' scar (`fork`'s
`copy_nonoverlapping` of `LinuxState`). `linux::thread`'s `FRAMES`/`FPAREAS` already have
exactly this shape, and the FP-area comment in `user.rs` already gave the reason for keeping
multi-KiB state out of a struct that is copied per switch.

**Measured**: `RunCell` 840 -> 184 bytes, about 10 KiB of `.bss` back. **The prediction that
went with it was wrong and is recorded as such**: the hot path was expected to get cheaper
too, since every syscall dispatch starts `let cell = cells()[cur]` - but the icount path
lengths are flat (`p2_user_syscall_floor` and `p5_crosscell_roundtrip` unchanged to the
instruction, `p2_user_roundtrip` 345007 -> 343007 milliticks). The compiler was already
reading the fields it needed instead of copying the struct, so the copy being priced did not
exist.

The proof is `smp`'s E4 phase, restructured because the cost is now **two different things**
and only one of them is per context. It adds the first context alone and the next five
together: the first costs 1 FP area + 2 table frames, the rest cost exactly 1 each. One batch
would have conflated them and could not tell "the table is amortised" from "every context
allocates a table" - and the marginal 1 is the number that makes "the ceiling is the cell's
budget" a statement about frames rather than a slogan. Freeing the cell returns all nine,
because a slot-handback path that is not also a release path is the S1' leak and the table
was a second one of exactly that shape.

It also removed a latent wrong value nothing could see: `install_forked` copied the parent's
whole `vqp` array while setting `nvcores: 1`, so slots `1..` held live pointers into the
parent's other rings that nothing could reach and nothing cleared. A record makes that
impossible to write by accident.

### 9.4 `MAX_VCORES` is gone

9.3 left the constant bounding three things. Retiring them is what actually lifts the
ceiling, and it turned out to be four, because the largest one was hiding in plain sight.

**The 1 MiB E4 claimed to remove was still being paid.** `CELL_FP` was
`[FpArea; MAX_CELLS * MAX_VCORES]` - one 4 KiB area per *possible* context - and it stayed on
as a fallback after the areas became funded. So the static E4 exists to delete was still
there, in full, and the frame each context funds was on top of it. It is now a **single**
area, and single deliberately: sharing one between two live contexts is not a degraded mode,
it is the `SYS_YIELD` FP defect exactly (each core saving over the other's register image, no
fault, no log), so no size of fallback table is correct and keeping 256 of them only made the
bug rarer. `attach_entity` refuses to create a context whose area cannot be funded, so nothing
reaches it, and `user::fp_fallbacks()` is how "nothing reaches it" is *measured* rather than
assumed.

**The id stride was the real ceiling.** `entity_of(cell, v) = cell * MAX_VCORES + vcore` is
what E2 called "the identity, not a mapping", and that was true and worth having - but a
derived id is a **stride**, and a stride is a hard cap on contexts per cell that no amount of
funding can lift. Worse, the same arithmetic was written a *second* time, in `smp`'s placement
queue, which packed and unpacked `cell * MAX_VCORES + vcore` independently. Two places
deciding one thing is the defect class this whole section exists to remove, so the derivation
had already failed at the one job it was doing.

Ids are **allocated** now, by the same `EntityTable::create` the Linux side already used, and
stored in the context's own record. A stored id is not a second copy of a fact; it is a
handle, the way a page-table base is. The reverse direction needs no mapping either, because
the entity records `(cell, context)` itself - which is what lets the placement queue carry
entity ids instead of a second encoding. So E3's "two ways of obtaining an id" collapses back
to one: one allocator, one table, `create_at` and the reserved band deleted.

**Id 0 means "no context"**, and that is load-bearing rather than a convention: an id is
stored in a funded table, a funded table grows into *zeroed* frames, so the value a fresh slot
reads must be the one that means empty.

Reserving it immediately found a latent defect no allocation pattern had produced before, and
`verify/entity` found it rather than a boot: `Funded` grows by whole frames, and an all-zero
`Entity` is **not** `Entity::EMPTY` - `owner` and `inside` want `NO_CPU`/`NOT_INSIDE`, which
are `u16::MAX` - so a slot grown past but never written reads as "free, owned by CPU 0,
entered by CPU 0". Invariant I7 exists for exactly that and fired on nine scenarios. `create`
now initialises every slot a growth adds, which is the same rule 9.3 applied to `Vcore` and
`Wait`: **write the record, never trust the frame**.

`nproc::Proc`'s two arrays got 9.3's treatment at the same time - 656 bytes to 56.

**Measured, end to end:**

| | before | after |
|---|---|---|
| `RunCell` | 840 B | 184 B |
| `nproc::Proc` | 656 B | 56 B |
| per-cell scaffolding, `.bss` | 21,504 B | 1,408 B |
| `CELL_FP` fallback, `.bss` | 1,048,576 B | 4,096 B |

**The one bound that survived is a real one**, and it says so where it lives: each context
that wants a *mapped queue ring* needs `QueuePair::REGION_SIZE` of the cell's address space,
and the window is 4 GiB. That is `load::MAX_QUEUE_VCORES` now. `MAX_VCORES` was the wrong home
for it twice over - it made an address-space question look like a scheduler question, and it
bounded the contexts *without* rings by the same number.

What bounds a cell's context count now is the cell's own frame budget, refused cleanly by
`Funded::reserve` and `fund_fp`.

**Proven** by `smp`'s E4 phase with **25 contexts on one cell** - past where the constant was -
costing 26 frames (2 table frames once, then one FP area each) and returning all 27, on all
three ISAs; and by the entity round trip, which replaced "the id decomposes arithmetically"
with "the entity's `(cell, context)` and that context's recorded id agree, in both
directions". Four controls observed firing: skipping the grown-slot initialisation fails 9
`verify/entity` scenarios; restoring the stride in `entity_of` panics `smp`; not storing the
allocated id panics `smp`; not releasing the funded tail fails the frame oracle by name
(`returned 25, want 27`).

One test-side consequence worth recording, because it changed what a proof could ask: with one
allocator, "which of these entities are Linux contexts" stopped being an arithmetic question.
`linuxthreads` now counts contexts on the **thread** side and entities on the **table** side
and compares two independently computed numbers, which is a better proof than the id-band
filter it replaced - and writing it is what surfaced 9.5.

### 9.5 E3's last remainder: the cell-level Linux state

Two things, and the first was found by the second.

**A Linux cell held two entities for one execution context.** `thread::init_cell` sets context
0's frame to `user::cell_frame(cell)` - so the cell's vcore 0 and Linux context 0 are the
*same* context - and then allocated an entity for it beside the one `user::install` had
already made. Two names for one thing, which is the shape this whole section exists to remove.
It was invisible while native ids were derived and Linux ids allocated above them, because the
two simply lived in different ranges; 9.4's single allocator made it a counting discrepancy in
the first proof that looked. Context 0 **adopts** the cell's vcore-0 entity now: one creator
(`user::install`), one releaser (`user::free_cell`), and `detach_entity` clears the field
without releasing, because two owners of one release is how a double free gets written.

**`linux::Proc::state` cached what the entities already knew.** `Runnable` and `Blocked` were
two of its four variants, maintained by a wake scan that ran over every cell on every
reschedule and wrote the bit before the pick. A stale cache there is a hang (the cell is never
picked) or a spin (it is picked with nothing able to proceed). The enum is now `Free | Live |
Zombie` - the lifecycle, which is the part no entity can answer - and runnability is derived:

```
runnable(i)   = Live && (any context Ready  ||  any parked context satisfiable)
is_blocked(i) = Live && no context Ready
```

The wake scan is gone, and so is the `state = Blocked` write in `park_or_switch`: the calling
context is already `Parked` on its entity, which is what `is_blocked` reads. There is no window
in which the bit and the contexts disagree, because there is no bit.

The `any context Ready` half is `thread::any_ready`, a scan of **that cell's** contexts, not
`EntityTable::all_parked`, which answers the same question by walking every entity in the
machine - the wrong cost for a predicate a pick evaluates per candidate cell. That choice has a
consequence recorded below.

**Proven** by the whole Linux suite on all three ISAs, including the three strict
production-runtime gates (`linuxnode`, `linuxbun`, `linuxclaude`), which is the evidence that
matters here: this is the scheduler predicate Node, Bun and Claude Code are picked by. Control
observed firing: dropping the `satisfiable` half of `runnable` deadlocks `linuxproc`.

**An honest non-result.** The adoption fix is *not* load-bearing for the derivation, because
`any_ready` scans the cell's contexts rather than calling `all_parked` - restoring the
duplicate entity broke no other phase. It is still a real fix (an entity and an FP frame per
Linux cell, and a leak, since the adopted-vs-owned split means `detach_entity` does not release
context 0's), so `linuxthreads` asserts it **directly** - context 0's entity must equal the
cell's vcore 0 - rather than leaving a cleanup nothing checks. That assertion fires under the
revert: `cell 0: Linux context 0 holds entity 2 but the cell's vcore 0 is entity 1`.

### 9.6 `Elastic<T, N>`: the pattern, written once

By this point the inline-plus-funded-tail shape had been written **three** times by hand -
`user::CELL_VCORES`, `nproc::PROC_WAITS`, and the FP areas before them - and three more fixed
per-cell tables were waiting for it. Three hazards come with the shape, each easy to get right
once and easy to forget in the fifth copy:

- **growth arrives zeroed**, and all-zero is a valid `T` only by accident of layout (the
  defect invariant I7 caught in `sched::entity`, nine scenarios);
- **every slot-handback path must also be a release path**, or the frames leak until the next
  boot (the S1' scar, found twice);
- **the descriptor must never be raw-copied**, which is why these tables live beside the
  `Copy` per-cell record rather than inside it.

`mm::kmeta::Elastic<T, N>` is that shape as a type. `grow` writes the empty value over *every*
slot a growth added, not just the one requested; `reset` releases rather than clears; the type
is deliberately not `Copy`.

**Converted with it, measured:**

| table | was | ceiling | now |
|---|---|---|---|
| `CELL_GRANTS` | 24,576 B | `MAX_GRANTS_PER_CELL = 64` | 8 inline, tail funded |
| `CELL_RES` | 9,216 B | `MAX_RES_PER_CELL = 8` | 4 inline, tail funded |

The grant table is the one that had already been raised, 16 -> 64, for the tile battle tier,
with docs/TILES.md 12 recording "whether 64 suffices for the largest real cell is an open
sizing question". It is not a sizing question now.

**Proven** by `librheotilebattle` on all three ISAs, and the proof is deliberately not the
existing churn loop: that allocates and drops one grant at a time, so it never holds more than
one slot and would pass at any table size. The cell now holds **twelve at once**, past the
inline half, and asserts they are distinct buffers; the kernel side asserts the table grew into
frames charged to that cell and that the frames came back when the cell was freed. Two controls
firing - the inline half restored to the old ceiling of 64 (`the grant table never grew past
its inline half`), and the release removed from `free_cell`.

That second control was not hypothetical: **the release was genuinely missing**, a third
instance of the S1' shape in the same session. `free_cell` had been taught to release the
context tail and covered only that; the frame assertion caught it (`left: 2, right: 0`).

**Examined and refused: `MAX_CELL_CHANNELS`.** It looks like the same defect and is not. It
lives in `abi/`, so it is part of the `SYS_CONNECT` contract a cell compiles against, and it
sizes an address-space window (`USER_CHANNEL_VA + N * QueuePair::REGION_SIZE`) - each channel
needs a mapped ring region whether or not the slot array is elastic. That is the
`MAX_QUEUE_VCORES` category: a real resource bound, correctly a constant.

**Still fixed, and named rather than implied:** `MAX_OBJECTS = 512` and `MAX_CELLS = 16`.
`MAX_CAPS_PER_CELL` was the third and is done - see 9.7.

### 9.7 The capability table, and the question that held it back

`MAX_CAPS_PER_CELL = 256` was a `[CapSlot; 256]` per cell: at 16 cells, **131,072 bytes of
`.bss`**, by a wide margin the largest fixed table left and a hard cap on how many objects one
cell could ever reach. `CapSlot`'s all-zero pattern is already its empty value, so the
conversion itself was never the difficulty.

**The difficulty was ownership.** A `CapTable` has no cell at construction - a launcher
declares one and mints into it *before* `install` - so a growth can happen while the table is
still unowned, and `Funded::release` credits the owner recorded **at release time**. Charging
one ledger and crediting another is not a crash; it is exhaustion attributed to the wrong
cell, which is the single thing the per-owner ledger exists to get right.

Reading `Funded::set_owner` showed the existing behaviour was to *silently do nothing* when
frames were already held. Safe for the ledger, and wrong about the owner without saying so.

**The answer is to model the transfer.** `set_owner` moves the charge (`move_charge`, ten
lines over a per-owner counter the module already keeps), because the operation is real: a
launcher builds the table and the cell adopts it at `install`. That also deletes an ordering
rule - "set the owner before the first growth" - that a capability table cannot obey, and it
retroactively removes the same rule from the grant and reservation tables of 9.6.

Two further things fell out, both of which the compiler or a test found rather than review:

- **`copy_from` was `self.slots = other.slots`** - a raw copy of the whole array, which
  becomes a copy of a `Funded` *descriptor* the moment the table is funded: two owners of one
  directory frame, the S1' scar exactly. `Elastic` is not `Copy`, so the old line stops
  compiling instead of turning into a double free. It is a deep copy now, and **fallible**,
  so `fork` refuses with `-EAGAIN` rather than completing with a child missing authority it
  believes it has - the shape `dup_state`'s funded VMA copy on the very next line already had.
- **Release is scoped to the kernel's own tables.** A first version released the cell's table
  at `user::reset`, which is wrong: the harnesses mint into their table *before* calling
  `reset`, so it wiped capabilities that had just been created (observed - `security` failing
  with `the OP_NOP after the attempt completed with status 5`, BAD_HANDLE). A launcher's table
  is not the kernel's to reclaim; it holds its frames for the life of the boot, exactly as the
  array it replaced did, and is reused rather than regrown across runs.

**Measured**: `[CapTable; MAX_CELLS]` 131,072 -> 16,896 bytes.

**Proven** by `cap-invariants` on all three ISAs: **300 capabilities in one cell**, past the
old ceiling of 256 where the mint returned `TableFull`, costing 4 frames charged to that cell,
**moved** to the cell that adopts the table, and all returned on release. Two controls firing -
the inline half enlarged to 320 (`300 capabilities in one table cost 0 frames`), and
`set_owner` relabelling instead of transferring (`the previous owner is still charged 4
frame(s) after the table was adopted`).

The second control is worth recording as a method note: its **first** version did not fire,
because the test called `set_owner` on an empty table, where there is nothing to move and
relabelling and transferring are the same thing. The assertion had to be moved to a table that
had already grown - which is the only configuration the design question was ever about.

### 9.8 The largest static was not a `MAX_*` at all

Everything up to 9.7 was aimed at the ceilings *already named in docs*, which is a list of
what someone had previously noticed. Building `cargo xtask sizes` - which reads `nm` on a
built kernel rather than a constant in a source file - produced a different list on its first
run, and the top of it was:

| symbol | bytes |
|---|---|
| `linux::pipe::PIPES` | **1,048,960** |
| `arch::imp::SYSCALL_KSTACK` | 524,288 |
| `linux::LINUX_STATE` | 427,776 |
| `linux::inetsock::DGRAMS` | 131,520 |
| `mm::frames::REFS` | 131,072 |

`PIPES` is larger than every `MAX_*` table removed before it **put together**, and it carries
no `MAX_*` name, which is exactly why reading constants never found it: 16 pipes with a
`[u8; 64 * 1024]` ring inline in each, resident on all three ISAs whether or not a pipe was
ever opened.

**`Elastic` does not apply here**, and the reason is worth stating because it is the first
table in this sequence where the obvious shape is not expressible: a whole `Pipe` is 65,552
bytes and `Funded<T>` requires `T` to fit in one frame. So the *buffer* becomes the funded
thing rather than the record - `Funded<PipePage>`, one 4 KiB page per element, reserved whole
at `alloc` and released when the last end closes. Two byte accessors (`byte`/`set_byte`) are
the entire call-site change, because the ring had exactly two byte accesses.

The owner is the **running** cell rather than an argument. Every path that opens a pipe is a
syscall being serviced for that cell, so it is the creator by construction, and threading an
index through `FdTable::pipe2` and two `alloc_ring_pair` helpers would have been three chances
to pass the wrong one. The other end may be held by a different cell after `fork`; the frames
stay charged to the creator, which is what `release` credits when the last end closes.

**Measured**: `PIPES` 1,048,960 -> a descriptor per slot; the symbol no longer appears in the
ranking at all. An open pipe costs 16 frames plus a directory, charged to the cell that opened
it - which is *more* than the zero it used to appear to cost, and is the honest number: the
memory was always there, it was just unattributed and paid whether or not anyone wanted a pipe.

**Proven** by `linuxproc` on all three ISAs, straight after the P11 shell suite: 7 rings
funded across pipelines, `pipe2`, `dup2` and cross-cell fork pipes, and every frame returned.

Two method notes, both from controls that did **not** fire first time:

- The assertion was originally at the **end** of the kernel, where it is vacuous - the harness
  resets at the *start* of each run, so by the last phase (which opens no pipe) every ring has
  been released regardless. Moved to just after the suite, the control fires (`17 frame(s) are
  still held by pipe rings after every pipe closed`). This is the third time in this document
  that "the harness resets at the start of a run" has invalidated a check written at the end.
- The first attempt at the control patched `reset` rather than `close_end`, because both end in
  the same three lines. A control that edits the wrong function is indistinguishable from a
  control that does not fire.

**Two more from the same ranking.** `inetsock::DGRAMS` (131,520 B) is 8 endpoints x 8 queued
x a 2 KiB payload, held whether or not a UDP socket was ever bound; the queue is
`Funded<Datagram>` now - `Datagram` is 2,056 bytes and fits a frame, so no page wrapper was
needed - reserved on bind and released on close. And `FdTable::maps`, the 8 KiB
`/proc/self/maps` snapshot every cell carried whether or not it ever read its own memory map
(almost none do - it is JavaScriptCore's probe), is funded on first read, taking
`LINUX_STATE` 427,776 -> **296,704 B**.

The `maps` conversion is where the type system did the arguing: `FdTable` is `Copy`, so
putting a `Funded` inside it **stops compiling**. That is the S1' scar enforced by the
compiler rather than by review, and the resolution is the one `user::CELL_VCORES` already
uses - the funded storage sits beside the `Copy` record, keyed by the running cell.

**A control that could not fire, and what it taught.** The datagram check first summed
`frames_held()` over the endpoints. Deleting the release did not fail it - because
`close_dgram` overwrites the descriptor, so the frames are **stranded** and the thing that
named them is gone; a table-side witness then reports zero for a real leak. Stranding is
exactly the S1' shape and is invisible from the table. Measuring the *pool* instead was too
broad (a Linux cell's other metadata moves it by 23 frames), so the check counts **both ends
of the pair** - queues funded against queues released - which is the only witness that sees a
strand without being confounded. Control fires: `2 datagram queue(s) were funded and 0
released`.

**Examined and refused: `signal::ACTIONS`** (33,280 B). It looks like the same defect and is
not: it is `[[SigAction; NSIG + 1]; MAX_CELLS]`, and `NSIG` is the number of signals the
Linux ABI defines - a dense array fixed by contract, with no ceiling anyone can hit. The
`MAX_CELL_CHANNELS` category.

**And then the fd paths, which were the rest of `LINUX_STATE`.** `FdKind::Vfs` carried a
`[u8; PATH_MAX]` inline, so `FdKind` was 272 bytes and `[FdKind; NFD]` 17,408 per cell -
every descriptor paying for a path whether or not it was a file. Only *two* sites read it,
so the bytes moved to a per-cell funded table indexed by fd slot, grown to the highest slot
that needs one (slots are handed out lowest-first, so a few open files cost one frame and a
cell that opens none costs nothing). `LINUX_STATE` 296,704 -> **34,560 bytes**.

**That conversion creates an obligation `fork` did not have**, and it is the interesting
part: `dup_state` raw-copies `LinuxState`, so the child inherits `path_len` for every fd
while its own path table is empty. The child then reads a **zeroed path** and acts on it -
no fault, no log. The same shape as the VMA table beside it, which is why that one is
already deep-copied there.

**Nothing in the suite noticed.** Deleting the deep copy left `linuxproc`, `linuxtools` and
`linuxdyn` all green, so the claim "this copy is required" had no evidence. `forkdir.c` is
the fixture that gives it some: a directory fd inherited across `fork` and used **by-fd** in
the child, `getdents64` being the operation that re-enters the VFS by the stored path.
Control fires (`forkdir: child read no entries`).

Writing it produced one more note worth keeping. The first version had the *parent* read
first, "so a child failure cannot be blamed on the directory being unreadable" - and it
failed against correct code, because `getdents64` keeps a per-fd cursor the child inherits
and `lseek` does not reset it, so the parent's read left the child at end-of-directory. The
fixture was wrong, not the kernel. The child reads first now and the parent afterwards on
its own cursor, which keeps the original intent without the confound.

**The size ranking, end to end:**

| symbol | before | after |
|---|---|---|
| `linux::pipe::PIPES` | 1,048,960 | gone |
| `linux::LINUX_STATE` | 427,776 | 34,560 |
| `linux::inetsock::DGRAMS` | 131,520 | gone |
| `[CapTable; MAX_CELLS]` | 131,072 | 16,896 |
| `CELL_GRANTS` + `CELL_RES` | 33,792 | ~1,500 |

What is left at the top is `arch::imp::SYSCALL_KSTACK` (524,288 - per-CPU kernel stacks) and
`mm::frames::REFS` (131,072 - one refcount byte per frame). Both are real and sized by the
hardware, and both are marked as such rather than queued.

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
