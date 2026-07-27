# Substrate 2 - re-founding the cell substrate

**Status:** Draft v0.1. Design only - nothing in this document is built.
This is the deep analysis of why the current cell substrate fights modern
software, and the target architecture that replaces it. It changes **no
doctrine**: the capability model, the kernel object list, the queue ABI,
W^X, the no-OOM-killer rule and the admission rule all stand. What it
replaces is the bring-up scaffolding underneath them.

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
| Magic VA map | `load.rs`/`user.rs` | image 1-4 GiB, stack 8 GiB, mmap 12 GiB *bounded at* queue 16 GiB, file-mmap 20 GiB, channels 24 GiB, grants 32 GiB, ld.so 64 GiB - the unbounded mmap cursor already walked into the queue region once (ARCHITECTURE-DEBT.md 4.0 blocker 2's history) |
| `USER_VA_MAX = 2^38` | `user.rs:79` | Sv39's 256 GiB, imposed on all three ISAs; V8 alone reserves a 4 GiB cage + code ranges and real servers map terabytes of files |
| Single cooperative CPU | everywhere | SMP is bring-up only; reservations admitted, unenforced; a blocking Linux thread parks its whole cell |
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

A **`comparison/linux/`** methodology (the seL4-comparison precedent: same
hardware, same harness, published method) defines the axes - syscall batch
throughput, IO tail latency P99.9 under load, container cold-start and
density, scheduler responsiveness under mixed load (the BORE benchmark
set). Every "outperforms" claim in this tree must cite a run of it. Until
lab runs exist, the claim is "designed to, unmeasured" - never "does"
(TOOLING.md 4).

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

The per-core vcore scheduler is specified concretely, three classes:

1. **Reserved class - EDF** over admitted reservations (object 7).
   Unchanged math, finally enforced. Admission still refuses over-commit
   (the system-wide ledger already exists).
2. **Fair class - EEVDF** (earliest eligible virtual deadline first).
   Virtual deadlines give latency-sized timeslices with no global tick,
   which is exactly the tickless model SCHEDULING.md 1 already requires -
   and it is the scheduler Linux itself moved to, so the comparison in
   section 2 is like-for-like.
3. **Burstiness on top - BORE, adapted.** Each vcore carries a burst score
   derived from the CPU time it accumulated *before explicitly
   relinquishing* (parking on a queue completion, `SYS_WAIT_*`,
   `SYS_YIELD`, a channel receive). The score adjusts the vcore's EEVDF
   weight/deadline: interactive, bursty work (a keystroke handler, an HTTP
   accept loop, an event loop) preempts quickly; long CPU-bound work (a
   GEMM, a compaction) keeps throughput. This OS is unusually well-shaped
   for BORE: on Linux, "went to sleep" must be inferred inside a kernel
   that also runs the IO; here every relinquish is an explicit,
   already-counted syscall-boundary transition (the idle / ktimer / reactor
   counters), so the score is **observed, never guessed**
   (ENGINEERING.md 1). The same burst signal is exported read-only to the
   in-cell strand runtime as a hint, so both levels of the two-level
   scheduler (SCHEDULING.md 3) act on one metric.

This pillar is the concurrency 10x: the current ceiling is one core,
cooperative, with no responsiveness model at all.

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

**FlashAttention 2/3 with async - covered, one gap found.** FA2's shape
(Q/K/V tile blocks, online softmax, high arithmetic intensity) maps onto
the existing tile framework - the battle tier already runs an attention
block with paged-KV sharing. FA3's shape (async producer/consumer
pipelining, compute overlapped with data movement) maps onto strands +
channels + double-buffered `TileBuf`s + the 64-deep pipeline fence, with
real overlap arriving with vcores. **The gap**: online softmax needs fast
`exp`, and the tree has no math library for `no_std` cells - hence libm on
the shelf plus a SIMD-dispatched exp in `tile::simd` beside the GEMM
kernels. Proxy: a `flashattn` phase in the battle kernel - a scaled FA2
block, async-double-buffered across strands, bit-exact vs a naive
reference.

**Node.js and Bun (JIT included) - covered, two personality gaps found.**
Traced against the *measured* binary (ARCHITECTURE-DEBT.md 4.0: 262 MiB
ET_EXEC, AVX-512, 12.8 MiB stack, 41 startup syscalls):

- 262 MiB image -> demand-paged ELF (landed) + full-lower-half VA
  (pillar 2). V8 reserves a 4 GiB pointer-compression cage plus code
  ranges; Bun/JSC reserves large spans - the fixed magic regions are
  exactly what such reservations collide with.
- JIT -> the **dual-mapping decision**: the same frames mapped RW at one VA
  and RX at another (JSC supports dual-map JIT), so W^X stays
  constitutional *per mapping* and no `UserRwx` variant is added; the
  RW->RX flip path already works as the fallback. This resolves
  ARCHITECTURE-DEBT.md 4.0's "deliberately not decided" with the answer
  that keeps the constitution intact at full JIT speed. Mechanism: a
  second mapping of an existing grant with disjoint permissions -
  composition, no new object.
- AVX-512 -> pillar 4's CPUID-sized XSAVE.
- libuv threadpool + worker_threads -> `MAX_THREADS = 8` dies with
  pillar 1; threads become vcores (pillar 3).
- The event loop -> epoll/eventfd2 (landed) + the pillar-7 wheel (Node
  arms thousands of coarse timers).

**Gaps found**: (a) **`madvise`** - V8/JSC trim heaps with
`MADV_DONTNEED`/`MADV_FREE`; it is not dispatched today, and with demand
paging landed it is cheap to honour (decommit the range) - a named
personality slice. (b) **FP/SIMD state across a signal handler** is
documented as not saved (L5) - a JIT taking a profiling signal
mid-vector-loop corrupts itself; real the moment Bun runs, so it is
scheduled with stage S4, not left as a footnote. Proxy: the sysx-style
startup-trace replay fixture extended with madvise and a
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

- **S1 - funded metadata.** Statics -> typed slabs charged to cell budgets;
  no semantic change; every existing test green plus a new `substrate` test
  kernel: spawn cells/fds/channels past every old cap, exhaust one cell's
  budget and observe the attributable refusal while a sibling is untouched.
- **S2 - VA/VMA + ASID.** The per-cell VA allocator, per-ISA `USER_VA_MAX`,
  huge-page grants, PCID/ASID. Proof: a cell maps beyond the old 256 GiB
  map; `mmapx`-class collision tests pass with regions allocated, not
  fixed; switch path length drops measured by `bench`.
- **S3 - vcores + preemption + the wheel + metrics.** The multikernel
  scheduler (EEVDF+BORE+EDF), timer wheel, histogram pipeline - the
  scheduler needs both on day one. Proof: `schedidle`-class oracles across
  cores; a spinning cell no longer starves siblings (closes #27); N
  concurrent timers honoured in order; burst score assertions from counted
  relinquish events.
- **S4 - hard-float std + FP engineering.** Target flips, XSAVE
  optimization, FP residency, FP-across-signals fixed. Proof: `stdrun`
  gains a float-heavy phase; the librheoipc register-pattern proof re-run
  under preemption; a signal-under-SIMD fixture.
- **S5 - per-vcore queues + NVMe/NIC pass-through** (with DRIVERS.md D2).
  Proof: an iommu-contained storage cell drives its own NVMe queues off a
  live disk; per-vcore submission never crosses cores (counter-asserted).
- **S6 - NUMA pools + core classes.** Placement proven in QEMU
  (chosen-node assertions), P/E and latency measured at the lab.
- **S7 - workload gates.** Bun/Claude Code startup, a TLS echo server
  across vcores with jitter histograms, a Kafka-shaped append-log bench on
  one NVMe queue, an OCI bundle running a Node workload under a
  `PrincipalId`, the `flashattn` tile phase - all as gates, none as design
  inputs.

---

## 16. Honest deferrals

Cross-host substrate (the cluster continuum), Sv57/5-level paging beyond
detection, GPU/NPU engines executing graphs (ACCELERATORS.md owns it),
kernel FP brackets (designed, unbuilt), P/E and NUMA latency wins
(lab-gated), and the `comparison/linux/` numbers themselves - the thesis
stands unproven until the harness runs.
