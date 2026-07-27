# comparison/linux/ - results

Produced by `sh comparison/linux/run.sh`. Read `README.md` first: the two halves
report **different units on purpose** and must not be divided into each other.

## Environment

| | |
|---|---|
| Kernel | Linux 6.18.5, `PREEMPT_DYNAMIC`, EEVDF (mainline, **not** a BORE build) |
| CPUs | 4 |
| Virtualisation | Docker guest on a shared host |
| rheo-os side | `kernel/src/sched/{bore,vcore}.rs` compiled for the host, unedited |

This is **not** CachyOS and **not** bare metal. The Linux half is therefore a
harness demonstration, not a baseline - see the caveat below.

## Linux: scheduler wake-to-run delay

Time from a condition variable being signalled to the woken thread executing, with
CPU-bound hog threads saturating the machine around it.

| Load | P50 | P95 | P99 | max | jitter (P95-P50) |
|---|---|---|---|---|---|
| idle | 23,963 ns | 32,537 ns | 54,488 ns | 279,685 ns | 8,574 ns |
| 4 hogs on 4 CPUs | 1,571 ns | 1,971 ns | 3,038 ns | 29,823 ns | 400 ns |
| 8 hogs on 4 CPUs | 1,621 ns | 22,353 ns | 35,677 ns | 5,216,091 ns | 20,732 ns |

**The idle row being 15x slower than the loaded row is not a scheduler result.** An
idle vCPU in this container is descheduled by the outer host and enters a C-state,
so the wakeup pays to come back; keeping the machine busy hides that cost. Across
repeated runs the 4-hog P50 moved between 1,576 ns and 22,125 ns - more than a
factor of ten - on identical code.

The finding to take from this table is therefore about the environment, not about
Linux: **a trustworthy Linux scheduler baseline cannot be produced in this
container.** The harness is portable and unchanged on real hardware; the numbers
are not.

## rheo-os: the shipped EEVDF+BORE run queue

Intervening CPU-bound slices between an interactive vcore waking and being picked.
Deterministic - integer arithmetic over a scripted trace, identical every run.

| Load | P50 | P95 | P99 | max |
|---|---|---|---|---|
| 4 CPU-bound vcores | 0 slices | 0 slices | 1 slice | 4 slices |
| 8 CPU-bound vcores | 0 slices | 0 slices | 1 slice | 8 slices |

Supporting state, which is what makes the zero above mean something rather than
being an artefact of an inactive burst term:

| | interactive | CPU-bound |
|---|---|---|
| BORE score | 0 | 4 |
| weight | 10,240 | 4,193 |

- The interactive vcore keeps the base weight because it relinquishes voluntarily
  every time; the CPU-bound ones are demoted 2.4x because they are preempted and
  keep accumulating. That is BORE doing its job, and the scores are **measured from
  the trace**, not configured.
- The **EEVDF eligibility gate deferred a nearer-deadline vcore 25 times** (4 hogs)
  and **59 times** (8 hogs). This is the only direct evidence that the eligibility
  rule is doing something: without it the result would be indistinguishable from
  plain EDF.
- `RunQueue::pick()` over a 16-vcore queue costs **~48-69 ns** per decision on this
  host. That number *is* wall-clock and legitimately so - it is ordinary integer
  Rust compiled for the host, not a guest under emulation. It says what the ordering
  decision costs, not how fast the OS is.

## What this does and does not establish

**Does:** rheo-os's scheduler puts a waking interactive task in front of saturating
CPU-bound work at P50 and P95, with at most one intervening slice at P99, and the
BORE weight separation and EEVDF eligibility deferrals that produce that result are
observable rather than asserted. The same harness runs on any machine, CachyOS
included.

**Does not:** establish anything about rheo-os being faster than Linux, on this axis
or any other. The units differ; the Linux baseline here is untrustworthy; and no
rheo-os wall-clock number exists outside the hardware lab. Per `README.md`'s
standing rule the wording stays **"designed to, unmeasured"**.
