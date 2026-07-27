# comparison/linux/ - measuring against tuned Linux (CachyOS-class)

`docs/SUBSTRATE.md` states a performance thesis: outperform modern Linux **at its
best** - including CachyOS-class builds, which ship EEVDF plus the BORE burstiness
patch and aggressive tuning - and do it structurally rather than by tuning. This
directory is where that claim has to be earned or withdrawn. It holds the
methodology, the harnesses, and the numbers actually produced.

The short version of the current state, so nothing here is read as more than it is:

> **rheo-os is not measured against CachyOS today, and no number in this tree says
> it is faster.** One axis - the scheduler's ordering decision - is measurable now
> and is measured below. The rest is gated on hardware, because rheo-os runs only
> under QEMU TCG in this repository and TCG models no caches, no TLB and no branch
> predictor (`docs/TOOLING.md` 4). A wall-clock rheo-os number produced here would
> be a fabrication, not a measurement.

## Why not just run both and compare

Because only one of them can be run.

CachyOS is a Linux distribution: it needs to boot on the metal (or a VM with real
timing), and the comparison is meaningful only if both systems run the **same
workload on the same hardware**. rheo-os boots on QEMU TCG here. Putting a TCG
number next to a bare-metal Linux number and dividing them produces a ratio with no
physical meaning - one side has caches and the other does not.

The seL4 comparison in `comparison/README.md` solved this by measuring both sides
**in the same QEMU with `-icount`**, so the metric is instruction path length and
the emulator's lack of a memory hierarchy affects both sides identically. That trick
does not transfer: seL4 is a kernel that boots under QEMU, and CachyOS is a full
Linux distribution whose scheduler behaviour under `-icount` (a deterministic,
single-threaded emulation of a multi-core machine) would not be the behaviour that
makes it fast.

So this directory does three separate, individually honest things instead of one
dishonest one.

## The three things

### 1. The scheduler-ordering axis - measurable today, measured below

CachyOS's distinguishing scheduler feature over mainline is **BORE on top of
EEVDF**, and `docs/SUBSTRATE.md` pillar 3 deliberately adopts the same frontier.
That makes the *decision* comparable even when the clock is not: when an interactive
task wakes while CPU-bound tasks are runnable, does it get the CPU, or does it queue
behind them?

Both sides answer that question here:

- **`sched_latency.rs`** measures the host Linux scheduler's **wake-to-run delay** -
  the time from a condition variable being signalled to the woken thread executing -
  with 0, N and 2N CPU-bound hog threads saturating the machine. Real threads, real
  scheduler, real nanoseconds.
- **`rheo_sched.rs`** runs rheo-os's **shipped** run queue (`kernel/src/sched/bore.rs`
  and `vcore.rs`, included by `#[path]`, unedited) over the equivalent scenario and
  reports how many CPU-bound slices intervene between an interactive vcore waking
  and being picked, plus the BORE weights the two classes end up with.

**The units differ and must never be divided.** Nanoseconds on one side, intervening
slices on the other. What is comparable is the *shape*: a scheduler that puts the
interactive task in front shows near-zero queueing on either metric, and one that
does not shows the tail growing with the hog count.

Only the storage is shimmed on the rheo side: `Funded<T>` is a page-directory table
charged to a cell's frame budget, and a host process has no frames, so it becomes a
`Vec` that grows with zeroed slots - which is the module's own stated contract
(`kernel/src/mm/kmeta.rs`: `Copy`, no drop glue, zero-initialised on growth). The
six methods it provides are every one `vcore.rs` calls, so a seventh appearing
upstream is a compile error here rather than a silent divergence. The scheduling
arithmetic, the eligibility gate, the burst score and the weight table are the
kernel's own source, byte for byte. This is the same "include the shipped code" rule
`comparison/threads` and `comparison/tiles` already follow.

### 2. The axes that are gated on hardware

Named here so the gap is a list rather than a shrug. Each needs rheo-os running on
real silicon, which is `docs/TOOLING.md` 4's lab:

| Axis | Why Linux is the interesting comparison | Blocked on |
|---|---|---|
| Syscall / IO batch throughput | completion rings are native here, retrofitted as io_uring there | rheo on hardware |
| IO tail latency P99.9 under load | no page-cache copy on the grant path | hardware + a real NVMe |
| Container cold start and density | a cell bundle has no namespace machinery to set up | the OCI runner (SUBSTRATE.md pillar 9) |
| Scheduler responsiveness, wall clock | the axis measured in (1), in nanoseconds on both sides | rheo on hardware |
| Context-switch cost | strands are stackless; threads are not | partly done - `comparison/threads` |
| Memory bandwidth / NUMA locality | per-node pools vs the Linux allocator | hardware with >1 node |

`comparison/threads` already measures the one row that does not need rheo on
hardware: the strand executor is ordinary portable Rust, so it runs on the host
beside `std::thread`, goroutines and asyncio, and those numbers are real.

### 3. The standing rule

Every "outperforms" claim anywhere in this tree must cite a run of a harness in
`comparison/`. Until the lab runs exist, the wording is **"designed to, unmeasured"**
and never "does". This file exists partly to make breaking that rule awkward.

## Running it

```sh
sh comparison/linux/run.sh
```

Both halves build with plain `rustc` and need nothing installed. On a CachyOS (or
any other) machine the Linux half runs unchanged, which is the point: the harness is
the portable part, and the results below are only this container's.

## Reading the numbers in RESULTS.md

The container these were taken in is a 4-CPU Docker guest on a shared host, and it
shows: the Linux idle case is *slower* than the loaded case (~25 us versus ~1.5 us),
which is not a scheduler property at all - an idle vCPU gets descheduled by the
outer host and by C-state entry, so the wakeup pays to come back. The loaded numbers
are the meaningful ones and even they move by more than 10x between runs.

That is stated rather than smoothed over, because it is the actual finding: **this
environment cannot produce a trustworthy Linux scheduler baseline**, and the
deliverable from the Linux half here is the harness, not the numbers. The rheo half
is deterministic and does not have this problem - it is integer arithmetic over a
scripted trace, and it produces the same answer every run.
