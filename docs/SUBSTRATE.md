# Substrate 2 - re-founding the cell substrate

**Status:** Draft v0.2. **The S1-S4 mechanisms are built and proven; they are
not yet load-bearing.** This is the deep analysis of why the current cell
substrate fights modern software, and the target architecture that replaces
it. It changes **no doctrine**: the capability model, the kernel object list,
the queue ABI, W^X, the no-OOM-killer rule and the admission rule all stand.
What it replaces is the bring-up scaffolding underneath them.

## What is built, and what "not yet load-bearing" means

Built and proven by the `substrate` test kernel on all three ISAs, with the
whole pre-existing suite (62 kernels per ISA) still green:

| Piece | Where | State |
|---|---|---|
| Funded kernel metadata (pillar 1) | `kernel/src/mm/kmeta.rs` | **Built.** `Funded<T>` grows past every old `MAX_*`, charges its owner, rolls back a failed reserve, releases exactly |
| Per-ISA user VA ceiling (pillar 2) | `arch::USER_VA_TOP` | **Built.** x86-64 `2^47`, ARM64 `2^48`, RISC-V Sv39 `2^38` as its own floor |
| VA region allocator (pillar 2) | `kernel/src/mm/vaspace.rs` | **Built.** First-fit with guard gaps, overlap refused, mid-range release splits |
| ASID/PCID-tagged switches (pillar 2 / S2) | `arch::paging_activate`, `mm::AddressSpace` | **Built.** A cross-address-space switch performs **no** TLB maintenance: the ASID (ARM64, RISC-V) or PCID (x86-64, where the CPU has PCID+INVPCID - observed, not assumed) disambiguates. The flush moves to the two places that genuinely need it - a tag handed to a new root (`AddressSpace::new`) and a batch of **mutations** to an existing one (`AddressSpace::dirty`). `librheoipc` asserts flushes < switches; they were equal by construction before |
| Per-CPU primitives (pillar 3) | `kernel/src/smp.rs` | **Built.** `PerCpu<T>`, `cpu_index()` total, always compiled |
| Vcores - one cell on N cores (pillar 3) | `kernel/src/user.rs`, `smp::place_vcores` | **Built.** Per-vcore frame / FP area / claim; two vcores of one cell proven to overlap on two cores, `SYS_YIELD` proven to reach a sibling vcore with no address-space switch, and a vcore proven to **block** while its sibling runs (per-vcore block state; the `schedidle` `bSSSSSSSSB` oracle), all three ISAs. `SYS_EXIT` ends a vcore and the **last one out** ends the cell. A vcore that forks or takes a signal is not done (docs/SMP.md 10.0a) |
| Per-vcore queue pairs (pillar 5 / S5) | `kernel/src/user.rs` | **Built.** `SYS_DOORBELL` drains the calling vcore's ring and `SYS_QUEUE_INFO` reports it; two vcores of one cell proven to hold disjoint rings and each complete its own round trip on its own core, all three ISAs. The loaded-cell ring placement (`load::map_queue` per vcore) is not written - no loaded cell asks yet (docs/SMP.md 10.0a) |
| Hierarchical timer wheel (pillar 7) | `kernel/src/ktimer/wheel.rs` | **Built.** 64 concurrent deadlines honoured in deadline order beside the named-client slots |
| Per-CPU timer arbiter (pillar 3/7) | `kernel/src/ktimer/mod.rs` | **Built.** The single-owner invariant is now stated per core |
| Metrics histograms (pillar 7) | `kernel/src/metrics.rs` | **Built.** Integer-only percentiles, jitter fixed as P95-P50, lazily funded |
| BORE burst score (pillar 3) | `kernel/src/sched/bore.rs` | **Built.** Integer log2, nice-shaped range, EMA smoothing, fork inheritance |
| EEVDF run queue (pillar 3) | `kernel/src/sched/vcore.rs` | **Built as ordering logic.** Not yet dispatching |
| Per-CPU DRBG roots (10a) | `kernel/src/rng/` | **Built.** Derived, never copied |
| Hard-float userspace std (pillar 4) | `targets/rheo_os-*.json` | **Built.** All three targets flipped; std builds and links |
| `madvise` (pillar 4 / 10a) | `kernel/src/linux/mem.rs` | **Built.** Real decommit; `WIPEONFORK`/`DONTFORK` honoured by `fork` |

**Not yet load-bearing** is the honest part, and it is a deliberate ordering
choice rather than an unfinished edge. The new mechanisms exist beside the old
ones; the kernel's own paths have **not** been migrated onto them:

- The fixed tables (`MAX_CELLS`, `MAX_CAPS_PER_CELL`, `MAX_MAPPED_FILES`,
  `MAX_CELL_CHANNELS`, `MAX_THREADS`, the object table) are still fixed
  arrays. `Funded<T>` is what they will become; nothing has moved yet.
- The magic VA map is still the map. `VaSpace` is proven, and `load.rs` /
  `user.rs` / `linux::mem` still use their constants.
- No vcore is dispatched. `RunQueue` decides who *should* run; the cooperative
  scheduler still decides who *does*, and preemption does not exist.
- No second core schedules anything. SMP bring-up is unchanged (docs/SMP.md 9).
- `metrics` records nothing unless a boot calls `enable()`; no subsystem does
  yet.

That split is the point. Each mechanism is provable on its own against a
hand-computed oracle *before* anything depends on it, which is the only order
in which a change this size stays reviewable - and it means the whole
pre-existing suite passes **unedited**, which is the additivity requirement
(docs/ENGINEERING.md 8). Migration is stage S1'-S3' below.

Composes: ARCHITECTURE.md (objects, admission rule), SCHEDULING.md 1a (the
multikernel model - this doc builds it), CONCURRENCY.md (vcores - this doc
builds them), MEMORY.md (budgets, no OOM killer), DRIVERS.md (driver cells
run on this substrate), CONTAINERS-KUBERNETES.md + IDENTITY.md (pillar 9),
NETSTACK.md (pillar 7's transport learnings), POWER.md (core classes),
ENGINEERING.md (how every stage lands).

---

## 1. The problem - the substrate fights the software

Every collision between this OS and a real modern workload has had the same
shape: the *doctrine* was fine, and a piece of *bring-up scaffolding* was in
the way. The evidence, from this tree's own history:

| Friction | Where | What happened |
|---|---|---|
| `MAX_CELLS = 16` | `kernel/src/user.rs:305` | Raised 8 -> 16 for L6 fork; a Kubernetes node runs hundreds of pods |
| `MAX_THREADS = 8` per Linux cell | `kernel/src/linux/thread.rs` | glibc + rayon fit; Node's libuv pool + worker_threads will not |
| `MAX_MAPPED_FILES` 8 -> 64 | `kernel/src/linux/filemap.rs:45` | Raised for "a production binary's dozen-plus shared libraries" |
| Frame budget 96 -> 384 MiB, pool 128 -> 512 MiB | `user.rs`, `frames.rs` | Raised for glibc arenas + the real Claude Code; the ledger itself calls it "a limit raise, not a design change" |
| Object table 128 -> 512, grant table 16 -> 64 | `capability/mod.rs` | Raised for the tile battle tier |
| `MAX_CELL_CHANNELS = 4` | per-cell channel table | "4 clients is the fixed-array ceiling" (N4a, verbatim) |
| 5-slot timer arbiter | `kernel/src/ktimer.rs` | One slot per *purpose*; N2e already defers "two concurrent timer waiters in one cell" |
| 4-entry UDP/TCP/ARP registries | N4b bridge | Fixed statics again |
| Magic VA map | `load.rs`/`user.rs` | image 1-4 GiB, stack 8 GiB, mmap 12 GiB *bounded at* queue 16 GiB, file-mmap 20 GiB, channels 24 GiB, grants 32 GiB, ld.so 64 GiB - the unbounded mmap cursor already walked into the queue region once (ARCHITECTURE-DEBT.md 4.0 blocker 2's history), and the map was stretched *again* (mmap window raised to 80..252 GiB) so JSC's 128 GiB Gigacage would fit (GOAL-BUN) |
| `USER_VA_MAX = 2^38` | `user.rs:79` | Sv39's 256 GiB, imposed on all three ISAs; V8 reserves a 4 GiB cage + code ranges, JSC reserves a **128 GiB** Gigacage (now measured on this OS), and real servers map terabytes of files |
| Single cooperative CPU | everywhere | SMP is bring-up only; reservations admitted, unenforced. Per-context blocking (LINUX-COMPAT.md L4 `pblock`) has since landed - it is what made real Node.js complete - but the **measured** frontier stands: the real Bun binary issued all 205 of its syscalls from its main thread and aborted, because its spawned worker never got a CPU (GOAL-BUN, SMP.md 10.1). Syscall-driven concurrency works; true parallelism does not exist |
| Soft-float `rheo_os-*` std targets | `targets/*.json` | A *ported* native program computes floats slower than the same code run unmodified under the Linux personality - backwards |
| Eager paging, eager fork, fixed stacks | closed one by one | Demand paging, COW, stack-growth all had to be *retrofitted*; each was a real workload blocker first |

The pattern: each fix was honest and each was reactive. The caps were never
the design - they were the price of a kernel that is "allocation-free"
interpreted as "all kernel state lives in fixed global arrays."

**Root cause, one sentence: the doctrine is not what fights modern software -
the bring-up substrate is.** The design docs already promise the right end
state (SCHEDULING.md 1a: a per-core multikernel; CONCURRENCY.md: multi-vcore
cells with 100k strands; MEMORY.md 7: budgets and pressure, no OOM killer).
Substrate 2 is the plan that makes the built kernel match its own documents.

---

## 2. Two governing principles

**Workloads are gates, never design inputs.** Kafka, JSON, TLS servers,
Node/Bun, containers, FlashAttention - every workload named in this document
exists to *validate* the substrate, and must run well, but no mechanism may
be shaped for one of them. The test: every pillar below must remain
justifiable with all workload names deleted. A workload-specific fast path
is refused the way a vendor-specific kernel path is (ARCHITECTURE.md 5).

**Outperform tuned Linux structurally, not by tuning.** The comparison
target is modern Linux at its best - including CachyOS-class builds
(EEVDF + BORE, aggressive tuning). This design adopts the same scheduler
frontier (pillar 3) and then goes where a shared-kernel POSIX design
structurally cannot follow:

- no shared-kernel lock contention: per-core multikernel, cross-core work is
  explicit messages (pillar 3);
- no page-cache copy on the IO path: grants + registered buffers end to end
  (pillar 5);
- no fd/epoll readiness machinery under the async ABI: completion rings are
  the native syscall surface, not a retrofit like io_uring;
- strand switch ~12 ns vs OS thread switch in microseconds (measured,
  comparison/threads/);
- container isolation without namespace assembly: the cell is the primitive
  (pillar 9).

**`comparison/linux/` now exists** (the seL4-comparison precedent: same hardware,
same harness, published method). It defines the axes - syscall batch throughput, IO
tail latency P99.9 under load, container cold-start and density, scheduler
responsiveness under mixed load - and says for each whether it is measurable today
or gated on the lab. One is measurable today and is measured: the **scheduler's
ordering decision**, because CachyOS's distinguishing feature over mainline is BORE
on top of EEVDF and pillar 3 deliberately adopts the same frontier, so the
*decision* is comparable even where the clock is not. `rheo_sched.rs` runs the
shipped `sched/{bore,vcore}.rs` (unedited, only the frame-funded storage shimmed)
over a scripted interactive-plus-hogs trace and reports intervening slices, BORE
weights and eligibility deferrals; `sched_latency.rs` asks the host Linux scheduler
the same question in nanoseconds. **The units differ and are never divided.**

The rest is gated on rheo-os running on real silicon, and the reason is not a
scheduling problem: putting a TCG number beside a bare-metal Linux number produces a
ratio with no physical meaning, and the `-icount` trick that makes the seL4
comparison fair does not transfer to a full Linux distribution. Every "outperforms"
claim in this tree must cite a run of a `comparison/` harness. Until the lab runs
exist, the claim is "designed to, unmeasured" - never "does" (TOOLING.md 4), and
`comparison/linux/RESULTS.md` says so in its own conclusion.

---

## 3. Pillar 1 - self-funded kernel metadata (kills every `MAX_*` structurally)

Per-cell kernel state - capability tables, VMA lists, fd tables, channel
slots, thread/vcore contexts, FP/XSAVE save areas, filemap entries, poll
sets - moves out of global fixed arrays into **typed kernel slabs allocated
from frames charged to the owning cell's frame budget** (the seL4
retype idea, adapted to budgets instead of user-visible untyped objects).

- A cell that needs a 40-entry fd table pays ~one frame for it, out of its
  own budget. A cell that spawns 200 children pays for 200 cell records.
  The only global limit left is physical memory.
- Exhaustion is a per-cell, attributable refusal (`-ENOMEM` / a typed cap
  error), never a global "table full" that one cell inflicts on another -
  and never a panic (the F2 fallible-`frames::alloc` discipline extends to
  every slab).
- The kernel remains *heap-free* in the global sense: slabs are typed,
  fixed-shape object pools over whole frames, with no general allocator, no
  fragmentation surprises, and accounting per cell. This preserves what
  "allocation-free" was actually protecting (predictability, no hidden
  allocation on hot paths) while deleting what it accidentally imposed
  (system-wide fixed ceilings).
- Admission audit: no new object, no new verb. This is the internal
  representation of objects that already exist. The budgets are the
  MEMORY.md 1 split, finally applied to the kernel's own metadata.

Every row of the section-1 table except the VA map and the scheduler is
closed by this pillar alone.

---

## 4. Pillar 2 - address spaces: full lower half, no magic VAs

**Status: the allocator exists and is proven (`mm/vaspace.rs`, the `substrate`
kernel); the regions are not placed by it yet (migration S2', not done). One
piece has landed early because it was a live defect rather than a design
debt: every per-cell region now has a *ceiling*.**

The fixed map gave each region a start and no end - the cursors were bumps
with nothing above them. Two consequences, both real:

- `mmap_file`'s cursor started at 20 GiB and grew unbounded, and the shared
  cross-cell channel rings sit at 24 GiB. A cell that file-mapped 4 GiB in
  total would have had its next mapping placed **on top of its own channel**,
  silently replacing the ring two cells communicate through.
- `SYS_GRANT` reserves pure address space, so asking for terabytes is cheap
  and legitimate - and walked the cursor out of the ISA's user range, which
  surfaces as a fault at some unrelated address instead of a refusal.
- The anonymous-`mmap` cursor is **global**, not per cell, so the 4 GiB
  between its base and the queue ring at 16 GiB is consumed by every cell in
  a boot together; past it a mapping lands on a cell's own queue-pair region,
  which the kernel still holds a raw `QueuePair` overlay onto.

Each region is now bounded by its neighbour and refuses rather than
overruns, on the cell's own path and on the peer's in `SYS_GRANT_SHARE`. The
`security` kernel proves it on all three ISAs with the property that makes a
refusal *clean*: an over-large grant is refused **and** the ordinary grant
that follows still lands at the window base, so the refused request did not
advance the cursor past address space it never got. Observed failing with
the ceiling removed.

The map's **internal order** is now a compile-time property too, where it was
a comment: every growing region is asserted to end where the next begins, so
moving a base without moving the ceiling that names it does not compile.

Two honesty notes. The grant ceiling is *proven*; the anonymous-`mmap` one is
**not**, and cannot be by a single call - an anonymous mapping is frame-backed,
so any span large enough to cross its window is refused first by the per-cell
frame budget, and a span large enough to overflow the arithmetic is refused
before that. Reaching it needs gigabytes of *successful* mappings accumulated
across cells, which is exactly the hazard of a global cursor. A first version
of that proof asserted a refusal the existing overflow check was already
producing - it passed with the ceiling deleted - and was removed rather than
kept as decoration. And grants run to the top of the user range, which spans
the Linux interpreter's base: not a conflict, because a cell has one
personality and a Linux cell has no typed grants, but stated rather than
asserted, since the assertion that looks right there would be false.

This is the part of S2' that can be stated as a constant. The rest - giving
the regions to the allocator so the bound is a *result* rather than a second
hand-written number, with guard gaps and per-ISA ceilings - is what remains.
`VaSpace::reserve` is a global first-fit, so wiring it means first recording
the fixed placements (image, stack, queue, channel, the `.user` window) as
reservations so new regions are allocated *around* them.

**The map is now recorded, and `munmap` asks it instead of inferring.** A
per-cell `VaSpace` holds every region a cell is given - its queue ring, its
channel slots, its grants, its anonymous and file mappings - recorded as each
is established. `SYS_MUNMAP` used to decide what an address was by which
constant range it fell in, the inference this module's own header rules out
and one that is wrong the moment a region moves or a new one is added between
two others; it now looks the address up. The record is **load-bearing**, not
decoration: with the anonymous-region record removed the legitimate
mmap/write/munmap round trip is refused, observed. Records are given back when
a whole region is unmapped, so a cell that churns mappings cannot exhaust its
table.

Getting there needed the S1' lesson a second time, and it is worth writing
down because it recurred at a different scope. A first attempt charged each
cell's table to the cell and let it grow lazily - so the first `reserve_fixed`
took a frame, and that frame landed inside the per-operation frame-cost
oracles the suite asserts on, breaking the `security` kernel. *A funded
table's one-off growth must not land inside a per-operation measurement*, now
per cell rather than globally. Every cell's table is therefore funded **once
at boot** (`user::init_layouts`), which makes the cost a boot cost and is the
same answer S1' gave for the mapped-file registry - including the same honest
consequence, that exhaustion there names the kernel rather than the cell.

**And placement is now an allocation.** The four regions a cell asks for at run
time - a typed grant, a file mapping, an anonymous `mmap`, and the read-only
copy a `SYS_GRANT_SHARE` places in the *peer* - no longer land where a rising
cursor happens to be. Each is a `VaSpace::reserve_in(lo, hi, ...)`: first-fit
inside the region's window, with a guard gap either side, skipping any span
already reserved rather than stepping over it a page at a time, and rolled back
with `release_at` on every failure path so a refused request costs no address
space. The three bump cursors are gone, including the **global** anonymous-mmap
cursor that was shared across all cells - one cell's mappings used to move
another cell's addresses.

The proof (`security`, all three ISAs) asserts the property rather than the
mechanism, because a cursor and an allocator agree on the first two answers: the
first grant lands at the window base, the second is **guard-gapped** past it,
and after the first is freed the third **reuses its base**. Only the third is
load-bearing, and it is exactly what a rising cursor cannot produce - with the
release suppressed it lands at `+0xa000` instead, observed.

`reserve_in` is windowed rather than whole-space on purpose, and the reason is a
limit worth naming: the loader's own placements - the image, the interpreter,
the stack, the `.user` window - are still constants and are **not** recorded, so
a global first-fit would allocate straight through them. Recording those at load
is what removes the windows, and it is the last step of pillar 2's address
work.

**And the record is now the authority on what the kernel owns.** A caller-chosen
`MAP_FIXED` is the one request placement cannot protect against - the cell names
the address - so it is checked against a list of spans the kernel holds. That
list was a second copy of `load.rs`'s constants living in `linux/mem.rs`, kept in
step by hand; it now asks the cell's recorded layout
(`user::kernel_owned_overlap`). Two details are the substance rather than the
move itself. It is an **allow-list over `RegionKind` with no `_` arm**: a
deny-list answers today's question and defaults a *new* kernel-owned kind to
permitted, silently, at whatever commit adds it, whereas this form defaults it to
refused and makes adding a variant a compile error. And the record had to be
**complete before it could be the authority** - the first attempt broke
`linuxproc` immediately, because a Linux cell never maps a queue ring and so
never recorded one, so delegating to the record *lost* a check that the constants
had. The kernel-owned windows are reserved in `user::install` for every cell now,
mapped or not, which is also the truthful statement about them: those addresses
are the kernel's whether or not anything is there yet.

Honest about what this bought: the refusals are the same two, because the only
caller is the Linux `MAP_FIXED` path and a Linux cell holds no typed grant and no
device BAR. What changed is the rule and where it lives, not the behaviour.
`mmapx` asserts both spans rather than one, so a regression that dropped a kind
cannot hide behind the other; the channel half was observed failing with its
record removed.

**And the user half is each ISA's own now, not the narrowest one's.**
`USER_VA_MAX` was `2^38` on all three because RISC-V Sv39 has the smallest user
half and one portable number is simpler than three. It was the wrong number in
two distinct ways. It is a property of the *page-table format*, so it belongs in
`arch` - Sv39 is the floor **profile**, not a ceiling ARM64 and x86-64 must
accept, which is the distinction `arch::USER_VA_TOP` already drew for `VaSpace`.
And holding the wide ISAs to the narrow one cost something concrete: a modern
runtime reserves address space by the hundred gigabyte, JavaScriptCore's Gigacage
being a single 128 GiB `PROT_NONE` reservation, so the Linux `mmap` window had to
be squeezed into the 172 GiB left over and a *second* cage would not have fit at
all. On x86-64 and ARM64 the hardware was never the constraint; the constant was.

`USER_VA_MAX` is `arch::USER_VA_TOP` and the Linux `mmap` window follows it,
ending four GiB below - headroom left deliberately, because the F1 pointer check
refuses a span that *reaches* the ceiling, so a mapping placed hard against it
could not be read back through a syscall argument. The largest reservation a cell
can take goes from 128 GiB to **64 TiB on x86-64** (`2^47`) and **128 TiB on
ARM64** (`2^48`); riscv64 keeps 128 GiB, which is its hardware. Nothing the
loader places moved - every fixed region is asserted below the floor, so the
widening can only have added room a cell asks for at run time.

`mmapx` proves it by **probing** rather than by naming a number: it doubles a
`PROT_NONE` reservation until one is refused, asserts the refusal is `ENOMEM` and
that 128 GiB - the Gigacage, the capability a JS runtime needs - fits, and reports
the largest that did. The kernel-side oracle is the largest power of two the
window holds, computed from the same two constants placement uses. A hardcoded
size would now be right on one ISA and wrong on two, which is the point.

This surfaced a **real defect the old ceiling had been hiding**. `unmap_range`
stepped one 4 KiB page at a time, so unmapping was O(range) *regardless of what
was mapped* - and a reservation is exactly the case where almost nothing is.
Bun's Gigacage teardown was already 33 million four-level walks; against a
terabyte-wide window it stopped being a slow path and became a hang, observed
immediately as the probe timing out on x86-64. The fix is one conservative
per-ISA query, `arch::paging_unmapped_span(root, va)`: how many bytes from `va`
are *certainly* unmapped because a table above the leaf level is absent, and `0`
when only a leaf lookup can answer. The portable walker skips an empty gigapage
in one step instead of 262,144, and because the query never claims a *mapped*
span, a caller that ignored it would still be correct - only slow.

- A **per-cell VA allocator over a real VMA structure** (possible once
  pillar 1 exists - today's "no VMA list" is a metadata-space problem)
  replaces the bump cursors and the fixed region bases. Queue, channel and
  grant regions are *allocated* per cell with guard gaps; cells already
  learn their addresses through `SYS_QUEUE_INFO` / `SYS_CONNECT` /
  `ShareInfo`, so the ABI needs no change - the constants stop being
  constants.
- **`USER_VA_MAX` becomes per-ISA**: x86-64 47-bit lower half, ARM64
  48-bit, RISC-V Sv48/Sv57 where the hardware reports them - with Sv39
  remaining the floor *profile*, not the ceiling imposed on the other two
  ISAs. A V8 pointer cage, a terabyte file mapping, and a JIT code range
  stop competing for one 256 GiB map.
- **Huge pages** (2 MiB / 1 GiB) become a grant attribute for heaps, tile
  buffers, and framebuffers - the TLB is the scarcest cache a GEMM has.
- **ASID/PCID lifecycle**: address-space switches stop flushing the TLB
  (per-cell ASIDs on ARM64/RISC-V, PCID on x86-64). Together with pillar 3
  this is most of the context-switch budget on real hardware.
- Demand paging, COW fork, and fault-grown stacks (already landed) carry
  over unchanged - this pillar is where they stop being retrofits and
  become the memory model.

---

## 5. Pillar 3 - vcores made real: the multikernel, with EEVDF + BORE

The kernel control plane becomes **per-core** (SCHEDULING.md 1a, verbatim):
each core runs its own scheduler state, run queue, timer wheel and frame
cache; cores share no implicitly mutable kernel state; cross-core work is
explicit ring messages with IPI doorbells. A cell holds **N vcores**; the
strand runtime schedules strands over them (work-stealing inside the cell);
Linux threads map onto vcores so a blocking thread parks a vcore, never the
cell. Timer preemption closes task #27; a compute-bound cell can no longer
starve the machine.

The scheduler design is owned in detail by **SCHEDULING.md 11** (the
CachyOS/EEVDF/BORE production learnings, recorded on main) and the
implementation plan by **SMP.md 10** (phase 2, task #132). This pillar
adopts both rather than restating them; the load-bearing points:

1. **One deadline-ordered ready queue, not two schedulers**
   (SCHEDULING.md 11.3): reserved work carries *hard* EDF deadlines
   (object 7 - the admission math unchanged and finally enforced);
   best-effort work carries *virtual* EEVDF deadlines
   (`eligible + slice/weight`); residual work is the tail of the same
   order (a virtual deadline at +infinity). No priority axis exists to
   lie on, and no CFS-style fairness engine is bolted beside the
   reservation one.
2. **BORE burstiness as the weight term** (SCHEDULING.md 11.4): the burst
   score is `bitlen(burst_cycles >> offset)` - a cheap **integer log2**
   in a nice-like 0..39 range, EMA-smoothed, with a spawned/forked child
   **inheriting its parent's observed burst** so a build's children
   cannot swamp interactive cells. The evidence source already exists:
   the per-context blocking machinery (`thread.rs` `pblock` - the change
   that made real Node.js run) marks exactly "cycles since this context
   last voluntarily relinquished". Observed, never guessed
   (ENGINEERING.md 1), never a task-declared hint. The same signal is
   exported read-only to the in-cell strand runtime, so both levels of
   the two-level scheduler act on one metric.
3. **The dispatch policy is a pluggable seam** (the sched_ext lesson,
   SCHEDULING.md 11.2): one `Policy` seam selected per pool at boot - the
   `net` crate's hft/edge/warehouse/embedded precedent - not a scheduler
   baked into the kernel. One workload, one policy.
4. **Deliberately not taken** (SCHEDULING.md 11.5): cross-LLC global load
   balancing (the multikernel partitions cores; a global balancer is the
   shared mutable state 1a exists to avoid) and any periodic-tick fair
   scheduler (the timer wheel's one-shots drive timeslices, not HZ).
5. **The implementation gate is the SMP-safety audit** (SMP.md 10.2):
   every shared `static mut` gets one owner, one lock, or per-CPU
   partitioning *before* a secondary core schedules anything; `ktimer`/
   `net_rx`/`input` become per-CPU; the single-CPU build compiles the
   locks out, so the cooperative path stays byte-identical and remains
   the proof baseline. The cooperative pick order stays round-robin until
   preemption lands (SCHEDULING.md 11.6) - latency-awareness would change
   nothing measurable on one cooperative CPU and would break the
   deterministic proof oracles.

This pillar is the concurrency 10x, and it is no longer a projection - it
is the **measured frontier**: the real Bun binary loads, links its seven
libraries, builds its 128 GiB Gigacage, spawns its worker via `clone3`,
and aborts precisely because that worker never gets a CPU - all 205
syscalls issued from the main thread (GOAL-BUN, SMP.md 10.1). Node.js
completes only because its coordination happens to align with blocking
points. The distance between "Node runs" and "Bun runs" *is* this pillar.

---

## 6. Pillar 4 - hard float as the userspace default; the kernel stays FP-free on purpose

**Userspace goes fully hard-float.** The three `rheo_os-*.json` std targets
flip (x86-64: drop `+soft-float`, add `+sse,+sse2`; ARM64: drop the
softfloat ABI, add `+neon`; riscv64: `+f,+d`, `lp64d`), std rebuilds through
the existing `cargo xtask std-patch` pipeline. librheo cells are already
hard-float; the Linux personality already runs FP/SIMD binaries. This
closes the absurdity that a program *ported* to the native target computes
floats slower than the same code run unmodified under the Linux
personality. The `.user`-window programs (`user_progs.rs`, lsh) stay
soft-float - they must be free of out-of-line calls and constant pools,
which hard-float invites; a structural exception, stated.

**The kernel stays FP-free - a performance choice, not a safety blanket.**
It is the same choice Linux makes (the Linux kernel builds `-mno-sse
-msoft-float`; FP in kernel code is illegal outside `kernel_fpu_begin`).
If the kernel never executes an FP instruction, then no syscall, trap or
interrupt ever saves or restores the FP/SIMD file - the 0.5-2.5 KiB of
XSAVE/NEON/F-D state moves only at cell/vcore switches and the syscall fast
path stays register-only. A hard-float kernel would tax **every kernel
entry** to benefit code the kernel deliberately does not contain: the data
plane lives in cells; the kernel is a control plane with no math in it.
Three load-bearing reasons, ranked:

1. the per-entry save/restore tax above;
2. the single-owner invariant - `user::switch_native_cell` is the one FP
   swap site, which is what made the SYS_YIELD vector-corruption defect
   findable, fixable and provable (LIBRHEO.md "FP/SIMD across the native
   cross-cell switch");
3. the N4b codec posture - the integer-only tcp/cc/bbr stack links beside
   the kernel binary, and FP there would falsify the premise the
   save/restore proof rests on (NETSTACK.md 22).

The scheduler frontier independently confirms the choice: BORE's burst
score is a **bit-length (integer log2)**, not a float - it lands natively
in this kernel's no-FPU fixed-point discipline, unlike a CFS-style
`vruntime` that wants division (SCHEDULING.md 11.4). The most modern
responsiveness heuristic in production Linux needs no kernel FP at all.

**Escape hatch, designed now, built only on evidence**: a scoped
`kernel_fp_begin`/`kernel_fp_end` bracket (the Linux mechanism) for a
future kernel user that measurably needs SIMD (bulk copy, in-kernel
crypto). It saves the interrupted context's FP state on entry, so the
doctrine can bend locally without repealing the fast path globally.

**Then the userspace FP cost is engineered down**: XSAVE init/modified
optimizations (skip clean state); per-vcore FP residency - a vcore pinned
to a core swaps FP only on preemption, never on a syscall; save areas
sized by CPUID at boot (AVX-512 costs its 2.5 KiB only where it exists)
and allocated from pillar-1 funded metadata, not a static worst case.
The L5 gap - FP state not saved across a Linux signal handler - is
scheduled with this pillar (see the Node/Bun walkthrough, section 12).

---

## 7. Pillar 5 - queues as the only data plane, per-vcore

- **Per-vcore queue pairs** (the io_uring-per-thread shape): submission
  never crosses cores, completion lands where the strand parked. The wire
  format (`abi/`) does not change; the *count* stops being one-per-cell.
- **Syscall batching** over the ring - N file ops, one doorbell - and
  **registered-buffer grants** so the hot path never re-validates: the
  IO.md contract (inline small, by-reference large, zero-copy into mapped
  pages) applied uniformly.
- **NVMe queue-pair pass-through**: a storage service cell owns its NVMe
  submission/completion queues directly (the SPDK model), contained by the
  IOMMU. This is DRIVERS.md D2's device capability trio applied to Lane C's
  native NVMe driver - **they land as one capability, not two**. The block
  cache moves into the cell; the ext4 crate's async posture (FILESYSTEMS.md
  named it, waiting for exactly this) turns on.
- **NIC queue steering**: RX/TX queues steered per cell by flow context
  (the NETSTACK.md N6 inbound direction), so a server cell owns its own
  receive queues instead of sharing one kernel ring.
- **Doorbells become MSI/IPI-backed** where the hardware allows: a wake is
  an interrupt to the right core, not a poll - under pillar 7's
  "interrupts are optional" law where it does not.

---

## 8. Pillar 6 - NUMA-typed memory

- **Per-node frame pools**: the Inventory already splits memory regions at
  node boundaries (SRAT affinities); the allocator stops flattening them.
- **Grants carry node affinity** (a hint today in librheo's `mem`; it
  becomes real placement).
- **vcores follow memory** (SCHEDULING.md 6): the pillar-3 scheduler treats
  a vcore's dominant grant node as its home; migration off-node needs a
  reason (load, power - POWER.md 3 supplies the hysteresis).
- **Per-node kernel slabs** from pillar 1, so kernel metadata is local too.
- QEMU models NUMA topology but not its latencies; placement is proven in
  QEMU (the right node is *chosen*), the win is measured at the lab.

**Landed: node-affine frame allocation, and the hint stopped being a lie.**
The pieces were all present and disconnected. The Inventory recorded which node
each memory region belongs to; `SYS_GRANT`'s fourth argument carried a node hint
that librheo's `mem::reserve_on` has always sent; and the allocator served every
request from one rotating search over one pool. The hint's own comment said
"recorded but single-node in QEMU" - two claims, and only the second was true.
The second stopped being true the moment QEMU was asked for two nodes.

`mm::frames` now learns, at boot, which **pool frame indices** sit on which node
(`init_numa`, called from the boot sequencer *after* `hw::detect()` - the pool is
brought up inside `arch::init()`, before any firmware table has been read, so the
question is unanswerable at pool init). A range per node rather than a per-frame
node id, because the pool is one contiguous physical span and the firmware's map
is already split at node boundaries, so each node's share is contiguous by
construction: sixteen pairs of `usize` instead of a 128 KiB side table.
`alloc_on(node)` searches that range; `alloc` is untouched, so every pre-NUMA
caller is unchanged, and `NODE_ANY` routes straight to it.

A **preference, not a guarantee**, and the difference is counted. A full node
falls back to the pool at large and `numa_fallbacks()` records it: refusing
instead would turn a bandwidth question into an out-of-memory one, and
ARCHITECTURE.md 5 has no OOM killer to appeal to - but a node-affine allocation
that quietly lands elsewhere is a placement the caller believes it made and did
not, which on real hardware is a silent bandwidth cliff rather than an error. So
the degradation is reported, the treatment `net_rx`'s poll tiers and `input`'s
interrupt recoveries already get. `NODE_ANY` is *not* counted: nothing was asked
for, so nothing was missed.

The `numa` test kernel proves it against an oracle it cannot reach. QEMU is
launched with two 512 MiB nodes, so the boundary is the first 512 MiB of RAM - a
number the test knows because it is how the test launched QEMU, and RAM base
comes from the one documented relationship in `mm/frames.rs` (the pool sits
64 MiB into RAM on every ISA). Every assertion compares a physical address
against that, never against the ranges the allocator built for itself. Asserted:
the two ranges **partition** the pool (adjacent, starting at the base, covering
all of it - a gap would be frames no node can reach, an overlap frames two nodes
both claim, and neither is visible from a single successful allocation); the
derived boundary is the oracle's; 64 frames land on each node with **zero**
fallbacks; a run-dry node degrades to its peer with the loss counted exactly once
(the run-dry point is `node_free`'s exact count, not "eventually"); and the used
counter still agrees with the bitmap afterwards. Observed on x86-64 (2 nodes from
the ACPI **SRAT**, boundary `0x2000_0000`) and riscv64 (2 nodes from the **device
tree**, `0xa000_0000`), each running node 1 dry after its 16,320 free frames. The
placement assertion was observed failing - "asked node 1, got 0x84020000 which is
below the 0xa0000000 boundary" - with `alloc_on` reverted to `alloc`.

**ARM64 skips with a measured reason.** QEMU hands a bare-ELF `-kernel` boot no
device-tree pointer in `x0` on `virt`, so no firmware source describes memory at
all and the built-in profile reports one node. That was checked rather than
assumed: passing `-dtb` explicitly does not reach `x0` either. The single-node
path is asserted *unchanged* instead - every range empty, `alloc_on`
degenerating to `alloc`, no fallback counted - so "NUMA landed" never quietly
alters a machine that has none.

Two things had to be got right that only exist once placement is real. **A default
must not be a decision**: `librheo`'s `Grant::reserve` passed node `0`, which was
harmless while the kernel dropped the hint and would now pin every default grant to
node 0 - a cell that never asked for a node, never falling back until node 0 was
full. It passes `NODE_ANY` now. And **widening a node's range is only sound while
its slices are contiguous**: `init_numa` builds each node's range by widening across
the regions that mention it, so a machine interleaving nodes inside one span (node 0,
node 1, node 0) would give node 0 a range that swallows node 1's - `alloc_on(0)`
handing out node 1's frames while reporting no fallback, a wrong answer that looks
like a right one. It is detected and the response is to know nothing rather than
something false: every range cleared, `alloc_on` degenerating to `alloc`, the reason
printed. Neither is reachable on QEMU's contiguous two-node layout, which is exactly
why they are worth writing down - the proof cannot catch them.

**And a cell's memory is co-located with the cell.** Placement is only worth having
if a cell's memory ends up in one place, so a cell now carries a **home node**,
stamped by the kernel at `install` - round-robin across the nodes the pool holds, so a
multi-cell workload spreads bandwidth instead of piling on node 0. Three things follow
it: its **kernel metadata** (page tables, capability tables, the VA record - `kmeta`
allocates on the owner's node, and holds the owner->node map itself so `mm` never
reaches up into `user`), its **typed grants** (a `SYS_GRANT` that names no node
resolves to the cell's own - "no preference" means "the kernel decides" and the kernel
decides locality, which is also Linux's default policy), and **every page it commits**
(`commit_range`, i.e. anonymous `mmap`, the Linux heap and stack, demand-page fills,
COW copies - the bulk of a cell's memory, so leaving this one anywhere would make the
property false of almost all of it). A spawned child and a forked child inherit the
parent's node; for `fork` that is not a preference but a fact, since COW starts the
child out mapping frames the parent already placed.

Proven in the same kernel, at the `kmeta` seam that every table growth passes
through: two owners given *different* nodes, and every funded frame of each - directory
and data - asserted on its owner's node, checked twice over, against `node_of` and
against the launch-derived boundary (`node_of` is built from the same ranges
`alloc_on` places against, so on its own it would only show the allocator is
self-consistent). Observed failing - "owner 6 asked for node 1; element 0 landed at
0x84063000, node 0" - with `kmeta` reverted to `frames::alloc`. Writing that proof also
found a real API hazard: `node_of` returned "no node" for any address that was not
frame-aligned, because `in_pool` asks a question about frames rather than addresses -
and the addresses a caller actually holds are interior ones. It rounds down now, so
"not on any node" cannot quietly mean "not aligned".

**And a core takes work from its own node first** - the CPU half of "vcores follow
memory". Placing a cell's pages is wasted if the core that runs it sits on the other
side of the interconnect, where an access costs roughly double. The published runnable
set is now **grouped by each cell's home node** with **one claim cursor per node**
(`smp::claim_next`), and a core tries its own node's cursor before any other; the
caller's ordering is preserved by a recorded permutation, so grouping is invisible above
the seam. It is the **same protocol replicated, not a new one**: each cursor is a single
`fetch_add`, so exactly one core can obtain each slot - the property the one shared
cursor had, and the reason it was chosen over scan-and-claim, since two cores entering
one cell is the failure docs/SMP.md 10.0 exists to make impossible. Work-conserving: a
core whose own group is exhausted takes remote work rather than idling. With fewer than
two nodes everything lands in one group and this is the single cursor, byte-for-byte.

Proven in the `smp` kernel, which now boots with two memory nodes and its CPUs split
across them (every pre-existing phase was verified to pass unchanged under that launch
before it was added). Measured: **7-8 of 8 cells run on a core of their own memory
node**, and the kernel's counters agree **exactly** with the node of the CPU that
actually ran each cell - which is what makes them evidence rather than decoration, since
a counter that missed the steal path shows up here as a mismatch.

**Three attempts, and the first two proved nothing** - worth recording, because each
looked right. (1) Asserting `local > 0` **passed with the preference deleted**: cells are
round-robin over two nodes and cores split evenly, so *random* claiming already lands
about half the cells locally. A ratio can never separate "the preference was applied"
from "the distribution happened to look local", and a threshold above chance is a guess
that becomes flakiness. (2) So the assertion became exact - *a core must never cross
while its own node still holds unclaimed work*, which by construction cannot happen -
but the control **passed again**, twice, for two different reasons: the detector read the
same `mine` binding the preference did, so disabling the preference disabled the
detector; and once that was separated, it judged crossings by the loop's `step`, which
counts distance from a starting group that is *itself* derived from the preference - so
with the preference off every core's start was group 0 and a node-1 core taking group 0
read as local. Judged from the group actually taken against a freshly-read `this_node()`,
the control fires. The lesson is the one this tree keeps relearning: a detector that
shares state with the thing it detects is not a detector.

Also worth recording: the crossing is counted where a core **wins the run-mark**, not
where it claims. A claim can be lost to a stealer, and counting at claim time recorded
nine claims for eight cells - so the counters disagreed with where the cells actually
ran, which is exactly the check that makes them worth keeping.

**Not yet, and named:** the cell-facing path is proven **at the kernel seam**
rather than from inside a cell - a cell cannot see a physical address, so
asserting placement from userspace needs the kernel to walk the cell's page
tables and report back, which is a harness rather than a stronger claim about the
mechanism; and the pmem pool has no node of its own. Latency is unmeasurable
here in any case: QEMU models the topology, not its costs.

---

## 9. Pillar 7 - a real metrics and timer pipeline; interrupts are optional by law

Sub-millisecond transport work (the CloudBridge BBRv3/FEC/QUIC field
results: jitter < 1 ms held on real RU-EU paths) is impossible without two
things this substrate currently rations: **plentiful cheap timers** and
**percentile discipline**.

**Timers.** The fixed 5-slot `ktimer` arbiter and the single reactor timer
slot per cell are named limits (N2e already defers "two concurrent timer
waiters in one cell"). Substrate 2 replaces the slot table with a
**hierarchical timer wheel per core**, funded by pillar 1 so deadline count
scales with memory, keeping every arbiter invariant: one owner of the
hardware one-shot per core, nearest-deadline arming, no client's cancel
loses another's deadline, deadlines in the timer's own ns domain. One QUIC
connection needs RTO/PTO + pacing + idle + key-update deadlines; N
connections need a wheel, not slots. Userspace gets multiple concurrent
timer waiters per cell (the reactor's timer slot becomes a funded table).

**Metrics.** A per-core, allocation-free **histogram pipeline**
(log-bucketed, HDR-style) for the numbers that decide designs: RTT
P50/P95/P99, jitter (**defined as P95 - P50, one definition everywhere**),
goodput, bufferbloat factor (avg_rtt/min_rtt - 1), Jain fairness across
connections and vcores - plus the existing honesty counters (halts, spin
polls, escalations, preserved deadlines) promoted into it. Exported as
typed event streams (object 4), never log lines; `cargo xtask bench` gains
percentile gates beside icount. Pacing safe-zones and burst scores become
*measured*, not tuned by feel.

**Interrupts are optional - a principle, not a workaround.** This tree
learned it three times: the x86 QEMU NIC with no usable RX line, the inert
x2APIC MSR block, QEMU device loopbacks that do not drive the interrupt
controller. Stated as law: every wake path has three modes - device
interrupt, timer-backed idle, bounded adaptive poll (the N2h hot/warm/cold
escalation) - **chosen by observation at bring-up, reported truthfully**
(`interrupt_driven()`, `did_idle()`), with deadlines honoured identically
in all three. Hybrid polling is a feature (NAPI/SQPOLL-style busy-poll
tiers for the hft/edge profiles), not an apology. DRIVERS.md's per-device
IRQ wait is a client of this law.

**Transport learnings recorded for NETSTACK.md (N7)**: BBRv3's dual-scale
bandwidth estimate and 0.85xBDP headroom as a `cc` refinement; pacing-rate
safe-zones validated against the jitter histograms; and an FEC phase - XOR
one-loss-per-group first, Reed-Solomon k-loss later - whose gate is
**`fec_recovered > 0` under group-aligned loss injection**, never a goodput
delta that might be something else (the CloudBridge writeup's own honest
caveat, adopted as the gate).

---

## 10. Pillar 8 - containers, Kubernetes, identity, and heterogeneous cores

The doctrine docs exist (CONTAINERS-KUBERNETES.md: the cell is the
container; IDENTITY.md: per-cell `PrincipalId`, SPIFFE-unified). Substrate 2
adds the *capacity* to actually run them:

- **A container is a cell bundle, not a new object.** An OCI/Docker or
  LXC/chroot workload = a capability bundle (cells sharing one budget and
  one `PrincipalId`) with a per-bundle VFS root (the mount table's
  per-session `/`, generalized). OCI image layers are served by DRIVERS.md
  Lane A as FUSE overlay mounts. Isolation is what cells already give -
  MMU + IOMMU + budgets - which is *stronger* than namespace assembly:
  there is no shared-namespace machinery to escape. chroot degenerates to a
  mount-table view.
- **Kubernetes maps onto what exists**: admission = grant minting,
  resources = frame budgets + reservations, NetworkPolicy = capability
  issuance on channels and NIC steering, pod identity = `PrincipalId`. The
  path is a native `rheo-runtime` implementing the CRI gRPC surface over
  N5a's h2 - not a ported containerd shim.
- **Big Node/Bun workloads inside containers stop conflicting by
  construction**: big VA rides pillar 2, high concurrency rides pillar 3,
  big IO rides pillar 5, and budgets make neighbors explicit - a bundle
  cannot take frames or vcores admission did not grant it. "Performing"
  and "not conflicting" are the same mechanism.
- **Heterogeneous cores (P/E) are first-class scheduler input**: the
  Inventory gains a per-core class (per-core CPUID/MIDR discovery already
  exists); SCHEDULING.md 2 pools bind to core classes (latency pool =
  P-cores, efficiency pool = E-cores); the EEVDF+BORE scheduler places by
  burst score - bursty vcores prefer P-cores, steady background work
  prefers E-cores - with POWER.md 3's energy policy supplying migration
  hysteresis. QEMU models no P/E asymmetry: designed now, lab-gated,
  stated.

---

## 10a. Cross-cut: the RNG on Substrate 2

The RNG design survives the re-founding almost untouched - the two-level
shape was right the first time: a kernel root ChaCha20 DRBG with fast key
erasure, seeded from the hardware source (RDSEED/RDRAND, RNDR) only after
SP 800-90B health tests, continuously reseeded, with **every**
`SYS_RANDOM`/`getrandom`/`/dev/urandom` read served by deriving a fresh
child DRBG from the root per call (`rng::derive_cell_drbg`) - so
kernel-served randomness holds no per-cell state that could go stale or be
duplicated; and in userspace, librheo's per-cell DRBG as a **library
call** (TIME-IDENTITY.md 4), seeded once over `SYS_RANDOM`, no syscall on
the hot path. Four touches, one per pillar that reaches it:

- **SMP (pillar 3)**: the multikernel rule - cores share no mutable
  kernel state - applies to the root DRBG. Each per-core kernel instance
  derives its own root from the boot root and reseeds it from the
  hardware source independently, so `getrandom` never takes a cross-core
  lock (the same per-CPU move SMP.md 10.2 makes for `ktimer`). Userspace
  mirrors it: one DRBG per **vcore** in the librheo runtime, so parallel
  strands never contend.
- **Fork duplication (pillar 2's COW) - the one real hazard**: COW `fork`
  duplicates a userspace DRBG's memory, so parent and child would emit
  **identical random streams**. Today it is latent: native cells only
  `spawn` (fresh crt0 seed - safe by construction), glibc's
  `arc4random`/`getrandom` go to the kernel root (safe), `/dev/urandom`
  is kernel-served (safe). It becomes real for Linux programs carrying
  their own userspace CSPRNG - OpenSSL being the big one, which protects
  itself with `MADV_WIPEONFORK`. That lands on the **`madvise` slice the
  Node/Bun walkthrough already scheduled**: `WIPEONFORK` zeroes the page
  at fork and OpenSSL reseeds itself. The RNG is that slice's second
  customer.
- **Hard float (pillar 4) doesn't touch it**: ChaCha20 is pure integer
  add-rotate-xor, so the FP-free kernel loses nothing. Cells can adopt
  integer-SIMD ChaCha (4-block parallel) through the existing
  `tile::simd` probe-dispatch pattern if throughput ever demands it.
- **Metrics (pillar 7)**: health-test failures, reseed counts, and the
  seed-source tier (hwrng vs documented floor) become typed events in the
  histogram pipeline, not log lines.

Honest deferrals: VM snapshot/clone resume duplicates DRBG state the same
way fork does - the answer is reseed-on-restore keyed off a generation
signal (vmgenid), owned by VIRTUALIZATION.md; and riscv64's documented
seed floor lifts when QEMU models the Zkr `seed` CSR.

---

## 11. The dependency policy, re-founded as tiers - and the library shelf

The blanket "no new dependencies casually" rule was right for the bring-up
kernel and now taxes exactly the userspace this substrate grows. It is
formalized into what the tree already does by precedent (`ext4plus` named
in FILESYSTEMS.md and adapted in its own crate; uutils built from source by
xtask):

- **Tier K - kernel/trusted** (`kernel/`, `abi/`, `posix/`, `xtask/`):
  **zero dependencies, unchanged, permanent.** The proof surface stays
  small. Register crates (`x86_64`, `riscv`, `aarch64-cpu`) stay out.
- **Tier S - system cells and foundation libraries** (`librheo/`, `net/`,
  driver cells, service cells): dependencies allowed when named in the
  owning doc with their transitive closure, version-pinned, vendored or
  hash-locked; anything that parses untrusted input is flagged as a
  supply-chain surface.
- **Tier A - applications and fixtures**: pinned crates.io dependencies
  freely, built from source by xtask, nothing binary in git.

**Shelf admission gate**: a library enters only if it builds and passes its
tests on **all three ISAs** in the posture that will use it (`no_std` for
Tier S), with any SIMD path carrying a portable scalar fallback (the tile
probe-and-fall-back pattern). An x86-only crate is refused outright -
TARGET-ARCHITECTURES.md 4 applies to dependencies too.

The shelf (each entry no_std-audited before adoption; hand-rolled code kept
as the oracle where one exists):

| Area | Library | Tier | Use |
|---|---|---|---|
| Async runtime + timer wheel | maitake / maitake-sync | S | A no_std timer wheel built for OS kernels - pillar 7's wheel's reference or basis; the strand executor stays ours |
| Lock-free structures | crossbeam-queue, st3 (work-stealing deque), concurrent-queue | S | Pillar 3 in-cell work stealing, cross-vcore queues |
| Collections | hashbrown, smallvec, arrayvec, slab, heapless | S | Funded tables without hand-rolling |
| Zero-copy ABI safety | zerocopy (or bytemuck) | S | Derive-checked `repr(C)` views over packets/descriptors; shrinks hand-audited `unsafe` outside the kernel |
| SIMD scanning | memchr | S | Beside our SWAR scan paths, A/B-tested |
| Math (no_std) | libm | S | Vectorized-dispatch exp/log (the FlashAttention gap, section 12) |
| Hashing | blake3, xxhash-rust, ahash | S | Content-addressed objects, fast tables |
| Crypto | RustCrypto (aes-gcm, chacha20poly1305, sha2, x25519, p256) | S | Cross-validates/replaces the N3a backends; our vectors are the acceptance test |
| TLS/QUIC oracles | rustls, quinn or s2n-quic, quiche | A | Host-side interop oracles - our stack must handshake with them in CI |
| FEC | reed-solomon-simd (or reed-solomon-erasure) | S | The N7 FEC phase, RS k-loss |
| Histograms | hdrhistogram (no_std fit to audit; else hand-rolled log buckets) | S | The pillar-7 percentile pipeline |
| Serialization | postcard, rkyv | S/A | Typed event streams, object metadata, zero-copy archives |
| Compression | lz4_flex, miniz_oxide, ruzstd | S | Object store, log shipping, OCI layers |
| Allocators | talc | S | A/B against the runtime free-list heap under fuzz |
| Filesystems | redoxfs, fatfs, rw-ext4 crates | S | The FILESYSTEMS.md drop-in route (precedent: ext4plus) |
| Virtio | virtio-drivers (rust-osdev) | S | Option for DRIVERS.md driver cells, judged against the in-tree drivers |
| Verification | kani, verus, loom, miri, proptest/arbitrary, cargo-fuzz | dev | Already doctrine; the shelf CI-wires them |

Refused even under the tiers: register crates in Tier K; tokio-shaped std
runtimes inside cells (the strand model is the point); any C library where
a pure-Rust equivalent is within reach (ruzstd-vs-zstd is the template:
pure-Rust decode now, revisit encode with evidence).

---

## 12. Workload walkthroughs - the design validated by simulation

Each walkthrough is a paper trace through the pillars with a named,
QEMU-runnable proxy kernel as its gate (the `librheotilebattle` precedent).
Meta-result first: **no pillar was added or bent for any workload** - the
section-2 principle held - and the walkthroughs caught three concrete items
(libm, madvise, FP-across-signals) that are now scheduled.

**Tile GEMM (7B-class layers) - covered.** `TileBuf` grants on huge pages
(pillar 2) -> strand-parallel `CpuExecutor` across vcores with work stealing
(pillar 3) -> SIMD dispatch (exists; AVX-512 lab-gated) -> hard-float std
(pillar 4) -> NUMA-affine buffers (pillar 6). EEVDF+BORE keeps a long GEMM
from starving interactive vcores: its burst score decays, preemption
exists. Proxy: `librheotilebattle` re-run with vcores > 1, same oracles.

**FlashAttention 2/3 with async - covered, and now built** (TILES.md 13).
The walkthrough found one gap - online softmax needs fast `exp`, and the
tree had no math for `no_std` cells - and predicted libm on the shelf. The
gap was real; the prediction was not taken. `libm` gives *correctly
rounded* `expf` over the whole domain, and a softmax needs neither half of
that (its argument is `x - rowmax`, always non-positive and bounded, and
the result is immediately divided by a sum of such results), while what a
tile kernel *does* need is a function that inlines and vectorises rather
than an opaque call per element. So `tile::fmath` is ~40 lines of range
reduction plus a degree-6 series with a **stated** error bound, asserted
against hand-computed values.

`tile::attn` then carries FA2 (the online-softmax block loop - no `Tq x Tk`
matrix ever exists) and FA3 (the same arithmetic pipelined over a
double-buffered staging pair), both proven in the battle tier on all three
ISAs. The load-bearing assertion is **block-size invariance**: the block
size is a tiling decision, the online rescale is what makes tiling not
change the answer, and a bug in that rescale shows up there and essentially
nowhere else. Honest: the pipeline's *overlap* is cooperative interleaving
until vcores run on more than one core, so FA3's structure is proven ahead
of the parallelism that pays for it.

**Node.js and Bun - no longer a paper walkthrough: both real binaries have
now run on the OS** (GOAL-NODE done, GOAL-BUN partial - LINUX-COMPAT.md 5),
and the measured results validate the pillars item by item:

- **Node.js v22 (124 MB, V8 + libuv) runs unmodified and exits 0** on
  x86-64: streamed off a live ext4 disk (~15,000 block-cache fills, never
  resident whole), seven shared libraries linked, V8 initialised, the
  event loop served, `rheo:42` printed. The real blocker was not a syscall
  but **per-context blocking** (a proc-level block used to park the whole
  cell, freezing the worker the main thread waited on) - which has landed.
  It runs `--jitless`: the one thing doctrine refuses in the whole 49-call
  trace is V8's single `mprotect(RWX)`.
- **Bun v1.3 (99 MB, JSC + Zig) loads to the concurrency frontier**: the
  full load path works - streaming, demand paging, dynamic linking, the
  **128 GiB Gigacage** as one `MAP_NORESERVE` reservation demand-filled
  (which forced the mmap window to be hand-raised to 80..252 GiB - the
  magic-VA map stretched yet again; pillar 2 deletes this class of edit),
  `clone3`, the event loop - then it aborts because its worker never gets
  a CPU. **That abort is pillar 3's absence, measured** (SMP.md 10.1).
- JIT -> the **dual-mapping decision** stands, now grounded in the trace:
  the same frames mapped RW at one VA and RX at another (JSC supports
  dual-map JIT; V8's equivalent is a write-then-flip RW->RX code space,
  which already works), so W^X stays constitutional *per mapping* and no
  `UserRwx` variant is added. `--jitless` is the honest interim - the
  interpreter runs today; the dual-map/flip JIT is what lets the engines
  *optimise* (LINUX-COMPAT.md names exactly this follow-on). Mechanism: a
  second mapping of an existing grant with disjoint permissions -
  composition, no new object.
- io_uring -> **refused `ENOSYS` deliberately, and that is the right
  answer**: libuv probes it and falls back to epoll+threadpool (the real
  Node trace shows exactly that fallback). This OS's native async ABI is
  the queue-pair ring (pillar 5); io_uring compatibility would be a second
  ring grafted beside it. The refusal is a design statement the traces
  now confirm costs nothing.
- AVX-512 -> pillar 4's CPUID-sized XSAVE.
- libuv threadpool + worker_threads -> `MAX_THREADS = 8` dies with
  pillar 1; threads become vcores (pillar 3).
- The event loop -> epoll/eventfd2/timerfd (landed) + the pillar-7 wheel
  (Node arms thousands of coarse timers).

**Gaps found**: (a) **`madvise`** - V8/JSC trim heaps with
`MADV_DONTNEED`/`MADV_FREE`; it is not dispatched today, and with demand
paging landed it is cheap to honour (decommit the range) - a named
personality slice (it also carries `MADV_WIPEONFORK`, which the RNG
section below needs). (b) **FP/SIMD state across a signal handler** is
documented as not saved (L5) - a JIT taking a profiling signal
mid-vector-loop corrupts itself; real the moment Bun runs full-speed, so
it is scheduled with stage S4, not left as a footnote. Proxy: the
sysx-style startup-trace replay fixture extended with madvise and a
signal-under-SIMD assertion.

**Kafka-class / HTTPS-TLS / JSON** (from the workload contract): the
append-log object + fencing leases + BBR pacing exist; the gate is a
partitioned append-log bench saturating one NVMe queue (pillar 5). TLS
serving composes N4a fan-out + N5a http/1-2 + N3b TLS; the gate is a TLS
echo server across vcores with the pillar-7 jitter histogram as evidence.
JSON is already proven (rheo-json); hard-float std removes its last tax.

---

## 13. Composition with DRIVERS.md

DRIVERS.md and this document are two halves of one story:

- **Driver cells run ON Substrate 2.** A FUSE/ublk/LKL driver cell is an
  ordinary cell: its tables come from funded metadata (an LKL cell's dozens
  of fds/threads/timers would blow every current `MAX_*`), its concurrency
  from vcores + the N4a one-strand-per-client shape, its data plane from
  per-vcore queues and registered buffers, its interrupts from the pillar-7
  wait modes - the DRIVERS.md per-device IRQ wait is a client of the same
  "interrupts are optional, report the mode truthfully" law.
- **DRIVERS.md D2 is the hardware edge of pillar 5**: NVMe queue-pair
  pass-through to a storage cell is D2's capability trio applied to Lane
  C's native NVMe driver. One capability, landed once.
- **Ordering**: D1 (FUSE) has no substrate dependency and can land any
  time. D2/D3 want S1 (funded metadata) first and benefit from S3 (vcores)
  without blocking on it - single-vcore driver cells work the way today's
  service cells do.
- **Containers close the loop**: OCI image layers ride Lane A FUSE overlay
  mounts - pillar 8 names it, DRIVERS.md owns it.

---

## 14. What does not change, and the honest 10x

**Unchanged**: the capability model and its proofs, the 8-object list, the
queue ABI wire format (`abi/`), kernel FP-freedom (pillar 4's terms), W^X
(strengthened by the dual-map answer), no OOM killer, the ARCHITECTURE.md 6
admission rule. This is a substrate re-founding, not a doctrine change -
and it proposes **zero new kernel objects and zero new verb families**
(pillar 1 is internal representation; pillar 2 re-homes existing regions;
pillar 3 implements documented scheduling; dual-map is a second mapping of
an existing grant).

**The 10x, quantified only where measurable**:

| Axis | Today | Substrate 2 |
|---|---|---|
| Cells / containers | 16, fixed | bounded by RAM (pillar 1) |
| Threads per Linux cell | 8, fixed | vcores x strands (pillars 1+3) |
| Cores doing work | 1, cooperative | per-core scaling, preemptive (pillar 3) |
| Concurrent timers per cell | 1 reactor slot (5 kernel slots) | wheel-bounded by memory (pillar 7) |
| User VA | 256 GiB fixed map, magic regions | full per-ISA lower half (pillar 2) |
| Cell switch | full TLB flush + eager FP swap | ASID/PCID + FP residency (pillars 2+4) |
| Native float | soft-float std | hard-float std (pillar 4) |

Wall-clock superiority over tuned Linux stays a *thesis* until
`comparison/linux/` runs on lab hardware say otherwise - "designed to,
unmeasured" until then.

---

## 15. Migration - staged, additive, each stage with its proof

Per ENGINEERING.md: every stage lands with pre-existing proofs passing
unedited, and names its own proof kernel.

**The mechanism stages S1-S4 below are built** (see the status table at the
top). What follows them is the *migration* - moving the kernel's own paths onto
the new mechanisms - which is where the caps and magic numbers actually
disappear. It is separated deliberately: building a replacement and switching
onto it are different risks, and only the second one can break a working
kernel.

- **S1' - migrate the fixed tables onto `Funded<T>`**, one at a time, each with
  the cap constant deleted rather than raised. Order by blast radius: the
  Linux `filemap` and channel tables first (self-contained), the per-cell fd
  and thread tables next, `MAX_CELLS` and the capability/object tables last
  (they touch the isolation proofs, so they land with the `cap-invariants` and
  `security` kernels re-run as the gate). **Done when:** no `MAX_*` capacity
  constant remains outside the accounting bounds, and a cell's table growth is
  refused as `-ENOMEM` attributable to that cell while a sibling is unaffected.

  **Landed so far:** the Linux **context tables** (`MAX_THREADS = 8` gone -
  `INITIAL_CONTEXTS` is a reservation, not a ceiling), the per-cell **signal**
  contexts, the per-cell **VMA list** (`MAX_VMAS = 128` gone), and the global
  **mapped-file registry** (`MAX_MAPPED_FILES = 64` gone, and its handle widened
  `u8` -> `u16` - the width was the real ceiling once the table could grow, and a
  wrapping handle would have pointed a mapping at another file's bytes, which is
  neither a fault nor a refusal).

  Three things the first migrations taught, all of them structural rather than
  incidental:

  1. **A funded table cannot be raw-copied.** `fork` clones a whole `LinuxState`
     with one `copy_nonoverlapping`, which duplicates a `Funded`'s *descriptor* -
     so parent and child would address one shared directory frame, every child
     mapping would appear in the parent, and whichever exited first would free
     frames the other still reads. Each funded field now needs an explicit deep
     copy, and a child whose table cannot be funded makes the `fork` fail
     (`-EAGAIN`) rather than run with a truncated copy.
  2. **Every slot-handback path becomes a release path.** There is no drop glue on
     a `Funded`, so a slot reused without releasing strands its frames. Two real
     leaks existed the moment the tables became funded: a reaped cell's context
     tables were only ever released by the between-runs reset (so every fork+exec
     pair - the `rsh` suite is twelve - leaked until the next boot), and
     `linux::reset` overwrote the VMA descriptor without releasing it.
  3. **A global table's one-off growth must not land inside a per-operation
     measurement.** Growing the mapped-file registry lazily charged its frames to
     whatever operation happened to be first, which broke `linuxrun`'s
     demand-paging assertion immediately (2 recorded pages against a load that
     "committed" 2 frames, both of which were the registry). It is now funded at
     its reset point, so its storage is a boot cost. A measurement that silently
     includes an unrelated one-off is worse than no measurement.
- **S2' - migrate placement onto `VaSpace`.** `load.rs`'s region bases,
  `user.rs`'s `MMAP_BASE`/`GRANT_BASE`/`FILEMMAP_BASE` and `linux::mem`'s
  window become allocations; `USER_VA_MAX` becomes `arch::USER_VA_TOP`. The ABI
  does not change (a cell already learns its addresses from `SYS_QUEUE_INFO` /
  `SYS_CONNECT` / `SYS_GRANT`). **Done when:** `mmapx`-class collision tests
  pass with regions allocated rather than fixed, and a cell reserves past the
  old 256 GiB bound on the two ISAs that have the room.

  **Landed so far: the ceilings, the recording, and run-time placement.** Each
  region has a named ceiling with a compile-time ordering assert; every region a
  cell is given is recorded in a per-cell `VaSpace` and `SYS_MUNMAP` looks the
  address up instead of inferring it from a constant range; and the four
  run-time regions - grant, file mapping, anonymous `mmap`, and the peer's
  read-only share - are placed by `reserve_in` with guard gaps and rollback,
  retiring three bump cursors including the global one. **And the ceiling is
  per-ISA**: `USER_VA_MAX` is `arch::USER_VA_TOP`, so the largest reservation a
  cell can take goes from 128 GiB to 64 TiB on x86-64 and 128 TiB on ARM64
  (riscv64 keeps its Sv39 128 GiB), with `unmap_range` taught to skip an absent
  gigapage in one step - without which a terabyte-wide reservation is a hang, not
  a slow unmap. The record is also the **authority** now: the `MAP_FIXED`
  kernel-owned check asks the cell's layout rather than a second copy of
  `load.rs`'s constants, as an allow-list over `RegionKind` with no `_` arm so a
  new kernel-owned kind defaults to refused. **Not yet:** the loader's own
  placements (image, interpreter, stack, `.user` window) are still constants and
  unrecorded, which is why `reserve_in` is windowed rather than a whole-space
  `reserve`.
- **S3' - dispatch through `RunQueue`.** The cooperative scheduler's pick
  becomes the queue's pick; every relinquish and preemption charges the burst;
  `metrics` is enabled at boot. Then preemption, then a second core - the SMP.md
  10.2 safety audit gating both. **Done when:** the `linuxbun` test flips from
  its accepted partial to `rheo:42`/exit 0, which is the measured frontier
  (SMP.md 10.1).

  **Landed: the queue dispatches, and a cell can be preempted on all three
  ISAs.** Two pieces, both additive:

  - `sched::dispatch` is the **seam**. The two `reschedule` functions and
    `SYS_YIELD` ask the ready queue for the *order*; the personality's own state
    (`PState`, the native process table) stays the sole authority on
    *runnability*, reconciled at the pick so the two can never disagree. With
    dispatch disabled, `pick` is the pre-migration round-robin expression for
    expression - the same trade SMP.md records for the `smp` feature, and what
    lets the migration be turned on one boot at a time. CPU time is charged and
    every relinquish recorded **at the transition itself**, which is what makes
    the BORE score measured rather than inferred: this kernel has no path from
    running to not-running that does not pass through a named call.
  - `sched::preempt` **takes the CPU away.** The arbiter gains a `Preempt` slot,
    the interrupt handler sets one flag, and the portable
    `user::on_user_interrupt` decides at trap exit whether the CPU moves - a
    sibling context of the same cell first (the `linuxbun` shape), then another
    cell. Splitting "note it" from "act on it" is not ceremony: an interrupt can
    land while the kernel holds a reference into a funded table, so a scheduler
    invoked from the handler would be reentrant.

  Per-ISA, each of which needed a real change: riscv64's user trap already
  serviced U-mode interrupts; **aarch64's lower-EL IRQ slot was a *fatal* slot**
  and cells ran with `SPSR.I` set, so neither the vector nor the mask existed;
  **x86-64 cells ran with `IF` clear** and the LAPIC stub saved only
  caller-saved registers, so the timer vector now routes through `common_trap`
  (reusing its ring-3 frame capture and IRET resume rather than writing a second
  one). Both mask changes are read at frame-construction time, so a cooperative
  boot's frames keep the pre-migration bits exactly.

  Proven by the **`preempt`** kernel, which carries its own negative control in
  the same binary: two cells run a compute loop that issues **no syscall at
  all**. Cooperatively, cell 0 runs all 24 rounds unbroken and cell 1 never gets
  the CPU - asserted, because that is what makes the other phase evidence of
  anything. With dispatch on, the shared order vector interleaves and the longest
  unbroken run drops from 24 to 2-9, with 14-33 slices actually taken. An
  interleave is only producible if something took the CPU away mid-loop.

  **Still to do for S3':** enabling dispatch for the *Linux* boots (it is proven
  for native cells and off by default everywhere), which is what the `linuxbun`
  gate needs; `metrics` enabled at boot; and the second core.

- **S1 - funded metadata.** Statics -> typed slabs charged to cell budgets;
  no semantic change; every existing test green plus a new `substrate` test
  kernel: spawn cells/fds/channels past every old cap, exhaust one cell's
  budget and observe the attributable refusal while a sibling is untouched.
- **S2 - VA/VMA + ASID.** The per-cell VA allocator, per-ISA `USER_VA_MAX`,
  huge-page grants, PCID/ASID. Proof: a cell maps beyond the old 256 GiB
  map; `mmapx`-class collision tests pass with regions allocated, not
  fixed; switch path length drops measured by `bench`.
- **S3 - vcores + preemption + the wheel + metrics.** The multikernel
  scheduler (the SCHEDULING.md 11.3 single deadline order: EDF hard +
  EEVDF virtual + BORE weights), timer wheel, histogram pipeline - the
  scheduler needs both on day one. The implementation plan is SMP.md 10
  (task #132), whose first deliverable is the SMP-safety audit, not a
  scheduler. Proof: `schedidle`-class oracles across cores; a spinning
  cell no longer starves siblings (closes #27); N concurrent timers
  honoured in order; burst-score assertions from counted relinquish
  events - and the exit gate is already written: the real Bun binary's
  `linuxbun` test flips from its accepted partial (exit 134, worker
  starved) to the strict branch (`rheo:42`, exit 0).
- **S4 - hard-float std + FP engineering.** Target flips, XSAVE
  optimization, FP residency, FP-across-signals fixed. Proof: `stdrun`
  gains a float-heavy phase; the librheoipc register-pattern proof re-run
  under preemption; a signal-under-SIMD fixture.

  **Landed: the target flips and FP-across-signals.** The three `rheo_os-*`
  std targets are hard-float (SSE2 / NEON / `+f,+d` with `lp64d`), and a
  signal handler no longer destroys the interrupted code's vector registers:
  delivery saves the FP image to the **user stack**, above the frame it
  writes, so nesting is handled by construction and `rt_sigreturn` restores
  its own level (docs/LINUX-COMPAT.md L5). The `sig_fp` fixture proves it on
  all three ISAs and is worth reading as a lesson in what a proof of this has
  to look like: **two earlier versions passed with the fix deleted**, because
  `raise()` is a call (caller-saved FP is already dead across it) and because
  a handler is an ordinary C function that *preserves* the callee-saved FP
  registers a register allocator would have chosen. Only inline asm on both
  sides makes the experiment an experiment. It also found a **fourth
  SYSRET-provenance defect** - `rt_sigreturn` rewrites its frame in place, so
  the frame-*pointer* test could not see that the register file had changed;
  the test is now the precondition itself (RCX == return RIP, R11 == RFLAGS).
  **Not done:** XSAVE init/modified optimization and per-vcore FP residency.
- **S5 - per-vcore queues + NVMe/NIC pass-through** (with DRIVERS.md D2).
  Proof: an iommu-contained storage cell drives its own NVMe queues off a
  live disk; per-vcore submission never crosses cores (counter-asserted).

  **Landed: the NVMe driver** (`kernel/src/hw/nvme.rs`, the `nvmefs` kernel,
  all three ISAs). This is the prerequisite the rest of S5 stands on, and it
  is worth saying why NVMe rather than more virtio-blk: virtio-blk is a
  paravirtual transport with one queue and a hypervisor behind it, while
  NVMe is what real storage presents - **paired submission and completion
  queues in host memory, a doorbell, out-of-order completion, one queue pair
  per core**. That last property *is* S5, and it is the same shape as this
  OS's own queue ABI, so the adaptation is a mapping rather than a
  translation layer.

  Bring-up is NVMe 1.4 section 7.6.1 (disable, publish `AQA`/`ASQ`/`ACQ`,
  enable, `IDENTIFY` namespace 1, `SET FEATURES` number-of-queues, create the
  I/O completion queue then the submission queue that names it), with reads
  and writes as `NVM READ`/`NVM WRITE`. It is also the tree's first device
  that **needs a BAR**: unlike virtio-pci, which this kernel drives through
  the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel precisely to avoid one, NVMe's
  register file *is* BAR0 - so `nvmefs` calls `hw::assign_pci_bars()` and
  maps the window, on machines where no firmware has done it.

  `nvmefs` is deliberately `blockfs` with the transport swapped, because that
  is the claim: `BlockDevice` is a seam, a second transport costs a driver
  and nothing above it changes a word. The same ext4 image, the same two
  files, the same byte-exact assertions, the same bounded cache proving the
  bytes streamed. It adds a **write round trip** - the read path alone would
  have left `NVM WRITE` reasoned-about rather than proven - taken on the last
  sector through a fresh handle (the cache would have answered from the line
  it just filled and proven nothing about the device), writing a pattern,
  reading it back, then restoring the original and reading *that* back, so
  the device has to return two different things for one sector in order. The
  drive is attached `snapshot=on`, so the writes genuinely reach QEMU's block
  layer while the committed fixture is untouched.

  **And the per-core data path is proven.** The driver creates one queue pair
  *and one bounce frame* per CPU, and a core submits on its own - selected by
  CPU index, not round-robin, not a hash. Two counters make that a measurement
  rather than an intention: submissions per queue, and submissions made on a
  queue the submitting CPU does not own. The `smp` kernel's NVMe phase has two
  cores read **different** sectors at the same instant (meeting at a rendezvous
  first, so the overlap is real) and asserts that two distinct queues took work,
  that each core's bytes are its own, and that **zero** submissions crossed a
  core - on all three ISAs.

  Two things had to be got right for that, and both were initially wrong in the
  same way - a fact that had been guessed rather than asked for:

  - **A core with no queue of its own is refused, not quietly given core 0's.**
    The first version counted the fallback and carried on, which reads as a
    sensible degraded mode and is not one: two cores on one ring is a data race,
    and it does not present as an error but as *wrong bytes*. It was found
    exactly that way - the same sector read back differently on round 3, no
    fault, no log. The counter now records something that must be impossible.
  - **`RefCell` was the wrong primitive**, and not stylistically. Its borrow flag
    is a plain `Cell`, so a `RefCell` is `!Sync` and a type containing one cannot
    soundly be shared between cores whatever the access pattern underneath - and
    this device *is* reached from two. It is a `SpinLock` now, never contended
    because of the partitioning, so an acquire is one uncontended atomic exchange
    next to a PCIe round trip. A `const` assertion that `Nvme: Sync` keeps a
    future field from undoing it silently. Same call `mm::frames` already made,
    for the same reason: whether a structure needs a lock is a property of the
    structure, not of which cargo features are enabled.

  **And the queue has depth.** A core issues up to 8 commands with **one
  doorbell**, each staging through its own frame from a per-channel pool, rather
  than paying a controller round trip per page. `nvmefs` asserts it with a read
  large enough to fill a batch, and checks the bytes as well as the count: every
  page of an 8-page batch equals the same page read singly, because a count alone
  would not catch a batch that mixed its pages up. Reverting the plan to one
  command per batch fails the assertion by name.

  The completion path is where NVMe stops resembling a paravirtual transport, and
  it is worth recording what is and is not true of it, because **two drafts
  claimed more than the evidence supported and both negative controls passed**.
  QEMU's controller genuinely reorders - all eight completions of an eight-deep
  batch arrive out of submission order, which the driver counts. The first draft
  said assuming submission order "would pass here and corrupt on hardware"; the
  second restructured the copy to happen per completion so the identifier would be
  load-bearing. Substituting the submission order for the looked-up identifier
  changed nothing either time, because each command's `PRP1` already names its own
  staging frame, so the data lands correctly however the reap is ordered, and a
  batch that waits for all `n` before returning does disjoint copies whose order
  cannot matter. What the identifier actually does here is bound the completion to
  this batch - a completion from outside it means the ring state is wrong and is
  failed on rather than counted as progress. It becomes load-bearing the moment a
  completion is acted on before its siblings arrive, which is what the interrupt
  path below will do. The second draft was reverted rather than kept, since it was
  more code for a property it did not yet have.

  **And completions raise an interrupt.** Every other wait in this kernel parks;
  this one could not, because a polled completion has no wake source. With MSI-X
  programmed there is one, and a waiting core halts instead of burning the
  microseconds a command takes - measured, 31 interrupts and 29-33 halts per run.
  x86-64 only for now: an MSI there is just a write to the local-APIC message
  region, while ARM64 needs a GICv3 ITS and RISC-V an IMSIC target, both real
  drivers. The other two poll **and say so**, and the test asserts the two can
  never disagree - halts with no interrupts, or interrupts with no halts, both
  fail.

  Three things this cost, each found by a control rather than by reasoning:

  - **A claimed interrupt that does not arrive is a hang, not a slow path.** The
    reap loop halts, so it never reaches its own deadline check. Masking the table
    entry turned a passing test into a 120-second timeout with no diagnostic. So
    the path is *verified by observation* before it is used - the probe-verify-
    fall-back pattern the LAPIC, the UART line and the PSCI conduit already use.
  - **The probe must not be able to halt either.** A first version spun waiting
    for the counter to move, with interrupts masked as the kernel always runs, so
    the vector could never be delivered and a working path reported itself
    broken. `arch::irq_window` opens a one-instruction window instead - bounded by
    construction, unlike `idle_wait`.
  - **A per-core queue needs a per-core interrupt.** With every queue's MSI aimed
    at the boot CPU, a secondary halted while the primary took its vector; the
    `smp` two-core phase caught it as a secondary that never finished. Each queue
    now has its own vector delivered to its own core, and each channel is verified
    **by the core that owns it**, against **that CPU's** interrupt count - a global
    counter would let a busy sibling answer the question, which is a check passing
    for the wrong reason.

  That per-core verification then earned its keep twice over, catching two more
  defects that every correctness check passes through:

  - **A secondary's local interrupt controller is not enabled by anyone.** The AP
    trampoline sets no APIC registers, and a core that never armed a timer has
    never software-enabled its own - so the MSI was correctly addressed and simply
    dropped. `arch::irq_ready_this_cpu` does the enable, split out of
    `enable_timer_irq_this_cpu` rather than reusing it, because that also writes
    `TMICT = 0` and would silently disarm whatever deadline the timer arbiter
    holds - the N2h class of defect, reintroduced by the convenient call.
  - **Eight table entries route nothing on their own.** The vector a completion
    queue raises is a field in its *create* command (`CDW11[31:16]`), and leaving
    it zero sends every queue through table entry 0 - so all eight vectors landed
    on the boot CPU. The `smp` phase now asserts `poll_fallbacks() == 0`, which
    fails by name when that field is reverted: a queue whose vector goes elsewhere
    still returns the right bytes, its owner just polls, so nothing else catches
    it.

  Both cores are now woken by their own completion vector, asserted.

  **RISC-V MSI was implemented and withdrawn**, which is worth recording because
  the result is a measurement rather than an unexplored gap. An MSI there should
  be a write into the IMSIC - each hart has a 4 KiB interrupt file, and storing an
  identity to it makes that identity pending on *that hart*, so the destination is
  chosen by which page the device writes to, a better fit for per-core queues than
  x86-64's address field. It does not deliver under QEMU 8.2's `virt`: with the
  table entry programmed to `0x2800_0000` identity 32, the entry **reads back
  correctly**, Message Control reads `0x8040_8011` (MSI-X enabled, function
  unmasked), and the hart's file has `eidelivery=1`, `eithreshold=0`, `eie0` bit 32
  and `sie.SEIE` set - yet after a completion `eip0` is still **0**, so the write
  never reached the IMSIC. The open question is whether QEMU routes PCIe DMA to
  that address on this machine, not whether the driver programmed it right.
  Shipping the path anyway would mean carrying device-programming code that
  provably does nothing in the only environment that can run it, on the strength
  of "it should work on hardware" - so it is `None`, the driver polls and says so,
  and the evidence is in `arch/riscv64/mod.rs` for whoever picks it up.

  **A GICv3 ITS driver was written for ARM64 and withdrawn too**, and it got
  further than the RISC-V attempt. Every command was consumed (`GITS_CREADR`
  caught up to `GITS_CWRITER` after MAPD / MAPC / MAPTI / INV / SYNC), LPIs were
  enabled on the redistributor over a shared 8 KiB configuration table and a
  64 KiB-aligned per-core pending table - both statics, because the frame
  allocator offers neither contiguity nor that alignment and adding a contiguous
  allocator for one device path would mean changing the tree's most
  safety-critical allocator. It also turned up a real defect worth keeping in the
  record: `GITS_TYPER.PTA` is **0** on this machine, so `MAPC`'s `RDbase` is a
  *processor number* and not a redistributor address - and the symptom of getting
  that wrong is indistinguishable from getting everything else wrong, since the
  commands are still accepted and the queue still drains. No LPI was ever taken.
  The open question is which mapping QEMU disagrees with, or whether the DeviceID
  the host bridge presents differs from the requester id - not whether the tables
  were published. ~250 lines of device programming that provably delivers nothing
  where it can be run is the untested claim this standard refuses, so ARM64 is
  `None` with the evidence in `arch/aarch64/mod.rs`.

  Both withdrawals share a shape worth naming: the per-ISA MSI seam
  (`msi_target` / `msi_route` / `irq_ready_this_cpu`) is now in place and the
  x86-64 implementation behind it is proven, so neither of these is a redesign
  when someone returns to it - it is filling in one function against a working
  contract, from recorded evidence rather than from scratch.

  **The IOMMU-containment gate is half proven, and finding out why cost two
  defects elsewhere.** `iommu` now runs the NVMe controller behind an identity
  domain and asserts the read succeeds - `NVMe read through the identity domain
  OK`. Getting there uncovered two pre-existing defects, both of the kind that has
  no symptom until a second device exists:

  - **`arch::mmio_map_window` mapped every caller at the same VA.** Correct while
    exactly one driver asks; the moment a second does, the second mapping replaces
    the first and the first driver's stored register VA silently addresses the
    *other device's* registers. It presented as the IOMMU's queued invalidation
    never draining - because its register writes were going into an NVMe BAR. The
    window is allocated per caller now, and exhaustion is refused rather than
    wrapped onto someone else's mapping.
  - **The VT-d queued-invalidation wait was unbounded.** `while IQH != IQT` with no
    deadline, which is what turned the above into a 120-second boot-test timeout
    with *no output at all* - no line naming the wait, the register or the
    subsystem. Bounded now, with the reason printed and the failure returned
    (docs/ENGINEERING.md 2: waits are deadlines, and a degraded path reports
    itself). The same fix went to the root-table handshake beside it.

  **And the containment gate is proven, both halves.** With an identity domain the
  NVMe controller comes up and reads correctly; with the domain revoked the DMA
  faults and the read fails. Same three steps as the virtio-blk phase beside it,
  because what is being shown is that containment is a property of the IOMMU rather
  than of the driver.

  The revoke half is also what made the driver's completion wait honest, and this
  is the third defect the gate turned up. A completion wait **halts**, and the only
  thing that ends the halt is the completion interrupt - raised by the same device
  whose DMA the wait depends on. Revoking the domain stops both together, so the
  halt had no wake source and the wait's own five-second deadline was never
  reached: the failure was a **hang, not a timeout**, and any wedged controller
  does the same on real hardware. The halt now carries an arbiter deadline of its
  own (`ktimer::TimerClient::Storage`), with `other_source = false` - `true` lets
  the arbiter halt on the device alone, which is the hang again, and was the first
  attempt. Where no hardware timer is up the arbiter declines to halt and the wait
  spins instead: slower, and the deadline stays reachable, which is the right way
  round. Proven by reverting to `park(true)`, which hangs at exactly that point.

  Adding the slot also caught a small hazard of its own: `ktimer::CLIENTS` is a
  hand-written count of a hand-written enum, and getting it wrong is an
  out-of-bounds index from a driver at run time rather than a compile error. It is
  asserted against the last variant now.

  **Not done:** the storage *cell* itself (DRIVERS.md D2 - a userspace driver
  owning the queues behind BAR grants and forwarded interrupts), and MSI on the two
  non-x86 ISAs. Transfers bounce through page-aligned frames one page per command, so
  `PRP1` addresses every command and no PRP list is built - correct, and the
  simple form on purpose, since a PRP list buys throughput TCG cannot show.
- **S6 - NUMA pools + core classes.** Placement proven in QEMU
  (chosen-node assertions), P/E and latency measured at the lab.

  **Landed: node-affine frame allocation** (`frames::alloc_on` + `init_numa`,
  section 8) with the `SYS_GRANT` node hint reaching the allocator instead of
  being dropped, proven by the `numa` kernel on x86-64 (ACPI SRAT) and riscv64
  (device tree) against a boundary oracle taken from the QEMU launch, and
  skipping with a measured reason on ARM64 (a bare-ELF boot gets no device
  tree - `-dtb` was tried). **And a cell's memory is co-located with the cell**: a
  home node stamped at `install` (round-robin, inherited by spawn and fork) that
  its kernel metadata, its typed grants and every page it commits all follow.
  **And the CPU half**: the runnable set is grouped by home node with one claim
  cursor per node, so a core takes its own node's work first (work-conserving,
  crossing rather than idling) - 7-8 of 8 cells on their own node in the `smp`
  kernel, with the counters agreeing exactly with the core each cell ran on.
  **Not yet:** core classes (P/E), which QEMU cannot model at all.
- **S7 - workload gates.** Real Node.js already runs to completion with its
  JIT enabled (GOAL-NODE), real Bun evaluates JavaScript and exits 0
  (GOAL-BUN), and the real Claude Code binary prints its version and exits 0
  (GOAL-CLAUDE), so the
  S7 gates move up a rung: a TLS echo server across vcores
  with jitter histograms, a Kafka-shaped append-log bench on one NVMe
  queue, an OCI bundle running a Node workload under a `PrincipalId`, the
  `flashattn` tile phase - all as gates, none as design inputs.

---

## 16. Honest deferrals

Cross-host substrate (the cluster continuum), Sv57/5-level paging beyond
detection, GPU/NPU engines executing graphs (ACCELERATORS.md owns it),
kernel FP brackets (designed, unbuilt), P/E and NUMA latency wins
(lab-gated), and the `comparison/linux/` numbers themselves - the thesis
stands unproven until the harness runs.
