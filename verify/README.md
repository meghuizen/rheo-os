# verify/ - host-side model checking of kernel state machines

Run it: `cargo xtask verify`. Seconds, not minutes; no QEMU.

## What this is for

Some kernel state machines are integer-only, allocation-free and dependency-free -
`sched/entity.rs`, `sched/bore.rs`, `sched/vcore.rs`. Those can be compiled and driven
**on the host**, at millions of operations per second, with every invariant checked
after every step. Each driver here `#[path]`-includes the shipped kernel source
verbatim and shims only the storage the kernel funds from frames, which is the same
rule `comparison/` follows: the code under test is the code that ships, not a model
of it.

Why it earns its place: five defects in the vcore and preemption work each needed four
cores and a 120-second QEMU boot to surface, and one of them presented as a wrong
syscall return value with no fault and no log. They are one defect - several places
must agree about an execution entity and none decides (docs/EXECUTION-MODEL.md 1). A
fuzzer over the state machine catches that class in milliseconds and shrinks the
counterexample to the two or three operations that matter.

## What it cannot do

It checks state machines. It does not check the trap path, the page tables, the FP
register file, or real interrupt timing - those need real cores and stay with the
in-QEMU test kernels. `cargo xtask verify` and `cargo xtask test` are both required;
neither substitutes for the other.

## entity/ - the execution entity

`verify/entity/fuzz.rs` drives `kernel/src/sched/entity.rs`: 20,000 random sequences of
400 operations over 24 entities and 4 CPUs, checking the invariants of
docs/EXECUTION-MODEL.md 5 after every step. The operations **are** the edges of that
document's ordering dependency graph (3.2), so coverage is measurable - and it is
**asserted**, not reported: a green run that never generated a steal, an entry refusal
or a quiesce has proven nothing and would read as if it had.

### The controls

A fuzzer that has never failed proves nothing. Each invariant was verified to fire by
breaking exactly one check in the shipped module and re-running. The seed and operation
index are from the actual runs.

| Check removed from `entity.rs` | Result |
|---|---|
| `park` accepts `NO_WAKE` | **I4** at seed 0, op 158 |
| `exit` leaves the wake source set | **I9** at seed 0, op 206 |
| `release` drops its live check | **I1** at seed 3, op 302 |
| `enter` stamps an out-of-range owner | **I2** at seed 0, op 16 |
| `pickable` refuses an *unclaimed* entity | **I5** at seed 0, op 35 |
| `steal` ignores `inside` (steals a **running** entity) | **I3** at seed 0, op 213 |
| `enter` drops **both** its occupied and owner checks | **I1** at seed 0, op 35 |

Seven invariants, seven firing controls. Two results are worth more than the pass:

- **`steal` ignoring `inside` is exactly "migrate a running entity"** - the capability
  attempted twice on real hardware in this branch and reverted twice, each attempt
  costing a full experiment (docs/SMP.md 10.0). The fuzzer names it in 213 operations on
  the first seed.
- **`enter`'s two checks are individually redundant and jointly load-bearing.** Removing
  either one alone still passes, because once an entity is entered `owner == inside`, so
  each refusal catches what the other would. Removing both fails at once. That is
  recorded rather than tidied away, because "remove one, the other covers it" is the
  reasoning that produced defect 1 - and it means a future change touching one of them
  must re-run this, not reason about it.

### The use-case scenarios

The fuzzer covers the state machine; nine scenarios cover the **use cases** - one per row
of docs/EXECUTION-MODEL.md 6, driven as a deterministic sequence with a hand-computed
expectation, so that table is executed rather than tabulated. They are not random: a
scenario asserts a specific outcome, and a random sequence that happened to produce it
would prove nothing about the shape being asked for.

| Scenario | The claim | Control that fires |
|---|---|---|
| threads of one cell across 4 cores | four entities of one cell run at once | (E4's fields; the model has no objection) |
| Node teardown: peer wakes a parked main | a cell with one parked and one runnable entity is **not** blocked | `all_parked` treating *any* parked entity as blocked - **defect 3 by name** |
| cell blocked when every entity is | blocked when all are, un-blocked by one wake | the same control, at the other end |
| last entity out ends the cell | live count falls 3, 2, 1, 0 | `live()` counting an exited entity |
| steal an idle entity, refuse a running one | the rebalance that ships works; the twice-reverted one is refused | `steal` ignoring `inside` |
| FA3 producer/consumer overlap | two entities of one cell running at the same instant | (E4's fields) |
| a bundle's two cells block independently | a bundle shares a budget, not an execution state | `all_parked` dropping its cell filter |
| create races a pick | a `fork` landing mid-scan cannot be entered twice | the `enter` controls above |
| budget exhaustion refuses cleanly | creation stops at the cap, table still consistent | (the cap; a panic here would be the OOM-panic MEMORY.md 7 forbids) |

Two of the nine have no control, and it is the same reason for both: they assert a
capability the **model** permits and the **kernel** does not yet hold the field for
(per-entity kernel stack and owner, stage E4). There is nothing to break in the model to
make them fail - which is the point of writing them now: when E4 lands, these are the
tests it has to satisfy, and they were written before the implementation rather than
after it.

### One defect the controls found in the checker itself

The first version of `check_i5` asked `EntityTable::pickable` whether work existed -
that is, it asked the code under test. So making `pickable` too strict (refusing an
unclaimed entity, which is a real bug shape: it strands work nobody has claimed) left
CPUs idle beside runnable entities and the check still **passed**, because both sides
agreed on a wrong answer.

That is the same defect the entity model exists to remove - two places deciding one
thing - reproduced inside its own test. The oracle now computes availability from the
entity's own fields and never calls `pickable`, and with that fixed the control fires at
op 35. Recorded because a passing test whose oracle is the implementation is worse than
no test: it reports confidence it has not earned.

## telemetry/ - the kernel's record rings

`verify/telemetry/fuzz.rs` drives `kernel/src/telemetry.rs`, the per-CPU non-blocking log
and event ring (docs/LOGGING.md 0).

**Why a fuzzer rather than a boot test**, which is the interesting part: the ring is
free-running `u32` counters masked into a slot array, so every case worth testing is a
wrap-around - `head`/`tail` crossing `u32::MAX`, a full ring, a ring emptied and refilled
across the boundary. A boot emits a few hundred records and reaches none of them. It would
pass on an implementation that is wrong after four billion messages, which is a number a
long-lived kernel reaches and a test never does. Here the boundary is *aimed at*: one run
starts the counters at `u32::MAX - 8` so the very first pushes cross it.

2,000 runs x 4,000 operations from 0, the same again across the wrap, and the per-CPU merge
at 1..8 CPUs - each against an independent `VecDeque` oracle, never against the ring's own
fields.

| Check broken in `telemetry.rs` | Result |
|---|---|
| `pending()` uses plain instead of wrapping subtraction | pending 0 vs model 2, at the wrap, seed 0 |
| `push()` indexes without masking | a record torn at the wrap, seed 0 |
| `pop_oldest()` takes the newest | merge order inverted, 2 CPUs, seed 1 |
| `pop_oldest()` takes the first non-empty ring | merge order wrong at record 2 |
| the truncated flag uses the clamped length | an over-long record not marked |
| a filtered record counted as a drop | 2 counted as dropped |

### Coalescing (docs/LOGGING.md 0.1)

The one idea taken from Arcan's shmif that this ring did not already have: a later record
whose value supersedes its predecessor is not new information, so fold it rather than fill
up and drop.

| Check broken in `telemetry.rs` | Result |
|---|---|
| fold without checking the record is still unread | an identical push after a read folded into the record already taken |
| fold ignores the payload | records differing in payload folded together |
| fold ignores the level | records differing in level folded together |
| fold ignores the CPU | records differing in CPU folded together |
| a reused slot inherits the previous occupant's fold counts | popped repeats 1, expected 0 |
| overflow counted globally but not folded into the stream | 0 losses in the stream, expected 7 |

Twelve controls in this driver, twelve firing. Two of them earned their keep beyond the
pass:

- **The reused-slot control did not fire at first.** The oracle was a queue of payloads, so
  a record carrying a previous occupant's `repeats` was invisible to it. It is a queue of
  `(payload, repeats, lost)` now, and the control fires at seed 0. A test that checks some
  of a record's fields reports confidence about all of them.
- **Coalescing broke the pre-existing wrap-around model at seed 0**, because the generator
  emits zero-length payloads and those legitimately fold - so the `VecDeque` oracle was
  comparing against a stream the ring no longer produced. That is the outcome to want: the
  model disagreed loudly instead of the change slipping through. Modelling the fold means
  the wrap-around test now exercises coalescing across the counter boundary too.

Six controls, six firing. The last one is worth reading: it **did not fire** at first.
`Rings::push` and `Rings::push_claimed` each carried their own copy of the buffered /
threshold / CPU-range checks, so breaking one left the other intact and the test called the
intact one. Two places deciding one thing, with a test unable to tell - the defect class
docs/EXECUTION-MODEL.md 1 exists for, reproduced inside the module written to demonstrate
the fix. `push` delegates now, and the control fires.

## bitmap/ - the free-frame search

`verify/bitmap/fuzz.rs` drives `kernel/src/mm/bitmap.rs`, the allocator's free-frame
search (docs/SMP.md 10.0g). It is here rather than in a boot test for a reason worth
stating plainly: the search went from a bit-at-a-time loop to a word-at-a-time one,
which is the *same answer* computed with four boundary conditions the old form did not
have - the first word's low bits, the last word's high bits, both at once in a
single-word range, and a range whose end is not a multiple of 64 - and every one of
those is a case where being wrong is **silent**. A missed free bit is a spurious
out-of-memory on a machine with free memory; a bit returned from outside `[lo, hi)` is
a frame on the wrong NUMA node, which `alloc_on` then reports as correctly placed.
Neither faults. A boot exercises a handful of bitmap shapes; this exercises 683,792.

`bitmap.rs` was written dependency-free *for* this - no statics, no `crate::` paths,
plain functions over a `&[u64]` - so it includes with no shim at all, which is the most
of-the-shipped-code any driver here gets under test.

The oracle is the **pre-change algorithm**, written out again here rather than
refactored out of the shipped one: a disagreement therefore means the optimisation
changed an answer, which is the entire question. Beside it, two properties that hold
even if the reference were also wrong - whatever comes back is inside the requested
range, and it is actually free.

Five sections: 16 hand-computed boundaries (each also checked *against* the reference,
so a wrong expected value in the table is caught rather than enshrined), 320,000 random
`find_in`/`find_from` cases across densities from empty to full, 32,000 random
`find_run`, and then **every 8-bit map exhaustively** over every `(nbits, lo, hi)` -
the one part that proves rather than tests.

| Change to `bitmap.rs` | Result |
|---|---|
| `low_mask` loses its `b >= 64` arm (`1 << (b % 64)`) | 2 named boundaries + thousands of random cases, all `got None want Some(_)` |
| the `hi` mask dropped | `single-bit range, taken` + "returned N outside [lo, hi) - on the NUMA path this is a frame on another node reported as placed" |

A sixth section reports the **cost**, and it is here rather than in `cargo xtask bench`
because the bench suite cannot see this change at all: the benches allocate from a
nearly-empty pool, where the rotating hint points straight at a free frame and both
algorithms stop on the first candidate. The win is on a *full* region - what `alloc_on`
faces once a NUMA node fills - and it is a step count, not an instruction count. Both
numbers are exact rather than sampled, since a bit-at-a-time scan examines every bit up
to and including the first free one and a word-at-a-time scan examines every word up to
that bit's: at 50/75/90/99% full over a 65,536-frame node that is 32,769/49,153/58,983/
64,881 bits against 513/769/922/1,014 words, **63x fewer steps**.

Wall clock was tried and is **not** reported: the `numa` kernel's boot went 11.6 s to
10.7 s, which is one sample of a boot that does far more than the run-dry phase, under
an emulator. The step count is what this container can defend.
