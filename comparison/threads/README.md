# comparison/threads/ - light threads vs Linux / Go / Python / .NET

The OS's concurrency model (docs/CONCURRENCY.md) claims threads get light by
splitting in two: the kernel schedules **vcores**, and a userspace runtime
schedules **strands** on them - stackless async tasks that "block" by parking
on a token and are woken by the queue-pair completion carrying that token. No
`clone(2)`, no kernel stack, no global thread table, no blocking syscall.

This directory tests whether that actually buys the promised performance,
against the runtimes the request named.

## What is measured

The **exact strand executor the OS ships** (`runtime/src/strand.rs`,
`include!`d verbatim) is run on the host and timed with the same framing as
comparison/rng: a strand is a userspace mechanism, so running its real logic
natively measures the mechanism's cost. Two numbers:

- **spawn + run + teardown** of a trivial task - "light thread, quick to set
  up and tear down."
- **context switch** - a cooperative `yield_now` handoff.

Baselines: Rust `std::thread` (the default Linux/Rust threading - one OS
thread per spawn), Go goroutines (best-in-class userspace light threads),
Python `threading` (OS threads) and `asyncio` (userspace coroutines).

```sh
sh comparison/threads/run.sh
```

## Measured results

Host: Intel Xeon @ 2.10 GHz, Linux 6.18.5. Representative of repeated runs
(absolute ns vary ~20%; the ratios are stable).

**spawn + teardown (lower is better):**

| runtime | ns per task | vs strands |
|---------|------------:|-----------:|
| **rheo-os strand** | **~85** | 1x |
| Go goroutine | ~670 | ~8x slower |
| Python asyncio task | ~13,000 | ~150x slower |
| Rust `std::thread` | ~100,000 | **~1,200x slower** |
| Python `threading.Thread` | ~140,000 | **~1,600x slower** |
| .NET 10 `Thread` | not measured (see below) | - |

**context switch (lower is better):**

| runtime | ns per switch | vs strands |
|---------|-------------:|-----------:|
| **rheo-os strand** | **~12.5** | 1x |
| Go goroutine (chan) | ~215 | ~17x slower |

**In-QEMU instruction path length** (deterministic, `cargo xtask bench`, the
`p4_*` lines, consistent across x86-64/ARM64/RISC-V): a strand spawn+teardown
is **~450 instructions**, a switch **~150 instructions**.

## Verdict: the concept holds

Strands beat the OS-thread model (Linux pthreads via Rust `std::thread` and
Python `threading`) by **three orders of magnitude** on setup/teardown - the
structural win of no `clone` syscall and no kernel stack. They beat Python
`asyncio` by ~150x (native state machine vs interpreter objects) and even Go's
goroutines - the reference light thread - by ~8x on spawn and ~17x on switch,
because a strand is *stackless* (no per-task stack to allocate) and the
cooperative single-vcore switch is just a re-queue. Async is native (every
strand is a `Future`), and the park-based `Mutex` never loses the vcore to a
held lock. This is the model the request asked for, and it delivers.

## Honest caveats

- **Mechanism cost, not a full workload.** As in comparison/rng, this times
  the mechanism (spawn/teardown/switch of trivial tasks) on real hardware. It
  is not a throughput benchmark of real concurrent work.
- **Single vcore.** The strand numbers are one cooperative scheduler on one
  core. Go and OS threads also parallelise across cores and preempt; the
  strand runtime does neither yet (multiple vcores + the preemption doorbell
  are future work, CONCURRENCY.md 4, BUILD-ORDER step 7). The comparison is
  fair for "cost to create/switch a unit of concurrency," not for CPU-bound
  parallel throughput.
- **Stackless vs stackful.** Strands cannot use a deep recursive call stack
  the way a goroutine or thread can; that is the trade that makes them cheap.
  Stackful strands (CONCURRENCY.md 2) are future work.
- **.NET 10 not measured.** No `dotnet` runtime is installed in this
  environment, so no number is reported rather than a fabricated one.
  Architecturally, `System.Threading.Thread` is OS-backed (same class as the
  `std::thread`/`pthread` result); `Task` on the thread pool is a userspace
  work item closer to the goroutine/asyncio class. The strand advantage over
  OS threads therefore applies to .NET threads too; the Task comparison would
  need a measured run to state.
- **Host numbers.** These are real-hardware wall-clock/cycle numbers for the
  comparison only. In-OS path lengths are the deterministic `p4_*` icount
  numbers above.
