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
