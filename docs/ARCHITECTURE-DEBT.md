# Architecture Debt Register

**Status:** v1.0, evidence-backed. Two independent read-only audits of the tree -
a **design/enforcement audit** (does the code implement the constitution it is
written against?) and a **dependency-graph + duplication audit** (is the
structure sound, and where has repetition earned a framework?). Every entry
carries `file:line` evidence and a classification, so this is a work list rather
than an opinion.

Companion docs: `ARCHITECTURE.md` §6 governs *what may enter the kernel*;
`ENGINEERING.md` is *how work lands*; `VALIDATION.md` is *what each profile must
prove*. This file is **what is currently wrong, and in what order to fix it**.

Every finding is classified:

- **(A) design flaw** - the architecture itself is wrong or incoherent
- **(B) unfinished** - the design is right, the code does not reach it yet
- **(C) honest deferral** - known, documented, acceptable

## 1. Closed since the audits ran

Recorded so the register stays honest about its own progress.

| Was | Now |
|---|---|
| **No user-pointer validation anywhere.** Every out-parameter syscall wrote through a cell-supplied address in kernel mode, with kernel RAM mapped supervisor-RWX in every cell root. Observed pre-fix: a cell wrote `[qp_va, cap_id]` into kernel `.bss`. | Fixed. `user_write_ok`/`user_read_ok` + typed helpers; every out-parameter, the whole queue payload surface, and the Linux dispatch bounded. Accept path is 10 instructions, zero memory accesses. |
| **`sys::mmap(1<<40)` panicked the kernel** - no cap, no capability check, no per-cell charge, and `frames::alloc` panicked on exhaustion. | Fixed. `frames::alloc -> Option`, a per-cell frame budget, a global reserve that keeps kernel allocations infallible. Refusal, not panic. |
| **`SYS_MUNMAP` freed frames it did not own** - no capability check at all; a peer's shared grant, the channel ring and the cell's own queue region were all freeable. Observed pre-fix: a cross-cell use-after-free was *accepted*. | Fixed. Routed through `grant_resolve` like its `COMMIT`/`DECOMMIT`/`SEAL` siblings. |
| **x86-64 had no working timer interrupt** (the x2APIC MSR block is inert under QEMU-TCG), so `SYS_ARM_TIMER` returned immediately and a "timer-backed idle" was a spin. | Fixed via xAPIC MMIO, selected by probe. Genuinely interrupt-driven on all three ISAs. |
| **Three "blocked" verdicts** - x86 AP bring-up, ARM64 PSCI, the x86 UART RX interrupt. | All three were **wrong**, from one root cause. A real secondary core now runs kernel code on all three ISAs. |

The lesson is recorded in `ENGINEERING.md` §1: when a capability lie is found,
every conclusion that touched it is a suspect.

## 2. Critical and high - correctness and enforcement

### 2.1 The capability model has no userspace surface (A + B)

`ARCHITECTURE.md` §3 names *mint / delegate / revoke*. None of the 49 syscalls is
a delegate, derive-subset, or revoke. `revoke_epoch`
(`kernel/src/capability/mod.rs:164`), `derive_subset` (`:325`) and `delegate`
(`:343`) have **zero production callers** - every caller is a test. Every
kernel-side mint passes `BUDGET_UNLIMITED`.

So object 2's claim to be "simultaneously the security model, the audit log, and
the metering system" is a **tested primitive with no production use**, and
`abi.rs:402`'s promise that "an epoch revoke kills the peer's copy too" is
unreachable. A cell cannot narrow a capability before passing it on.

### 2.2 Revocation is vacuous for memory grants (A)

`revoke_epoch` bumps `Object::epoch`, which makes future *grant checks* fail. But
after `SYS_COMMIT` a cell reaches its pages **through the MMU, not a grant
check**, and `grant_share` (`user.rs:648`) maps the same frames into the peer.
Nothing unmaps on revoke. §8.2 property 3 therefore holds **vacuously**: no
capability check stands between the peer and the bytes.

The machinery to fix it exists (`AddressSpace::unmap`,
`paging_for_each_user_leaf`); property 3 also needs restating to mean what the
design needs.

### 2.3 A spawned cell shares its parent's capability table (A + doc divergence)

`install_spawned` copies the parent's pointers verbatim
(`kernel/src/user.rs:1041-1042`). Consequences: `abi.rs:313`'s claim that spawn
authority is "not minted into a spawned child by default" is **false** - every
descendant inherits it; and §8.2 property 4 (disjoint capability sets) is
**inapplicable** to any parent/child pair, or to the whole service fan-out where
four cells share one table. What actually isolates memory is the per-cell grant
array plus the page tables - so in the shipped code the capability table is not
the isolation boundary the design says it is.

### 2.4 Kernel-blocking waits do not reschedule (A)

`IO.md:9` and `CONCURRENCY.md:24,38` state that blocking does not exist below the
library level, and §4.2 says scheduler activations are viable *"because no
blocking syscall exists"*. But `SYS_ARM_TIMER` (`time.rs:95-114`),
`SYS_WAIT_NET` (`net_rx.rs:443+`) and `SYS_WAIT_INPUT` all spin or park **in
kernel context without rescheduling**; only `SYS_WAIT` reschedules. One cell's
`sleep(1s)` idles the entire machine while siblings sit runnable.

Worse, `reschedule` **panics** when nothing is runnable
(`linux/proc.rs:491`, `nproc.rs:487`) - so a process blocked on the network,
which is by definition the only runnable thing, **cannot be expressed**.

This is the keystone: it is simultaneously the enforcement defect and rung 1 of
the compatibility ladder (§4). The shape needed already exists next to it -
`linux/proc.rs:476-534` has a `Block` enum, `satisfiable(i)`, `complete_block(n)`
- so adding `Block::{Epoll,Timer,Console}` is mechanical. What is missing is a
**scheduler idle state** composing `ktimer` + `net_rx` + `input`.

### 2.5 Admission cannot refuse over-commit (A)

`reserve_admit` tests only `cell_admission(cur)` (`user.rs:753`) against
`CELL_ADMISSION[MAX_CELLS]`. There is no system-wide ledger, so sixteen cells
each admitting 90% all succeed - 1440% of one CPU admitted, nothing refused.
Doctrine 5 ("accepted by math or rejected loudly") is scoped so that it cannot
reject the only over-commit that matters. Aggravating: a **second** global
admission controller exists for the same object (`svc.rs:21`, via the ungated
legacy `SYS_RESERVE`) which **discards its `Reservation`** (`svc.rs:73`), so it
can never release and its committed utilisation leaks monotonically upward.

### 2.6 Ambient authority, and a grant check that gates the wrong thing (A + C)

`svc::handle` (`svc.rs:49-169`) is reached for any native syscall the main match
does not claim and performs **no capability check on any of its 18 verbs** -
including the whole file surface, `SYS_RANDOM`, `SYS_LEASE`, and `SYS_MEMINFO`
(which leaks global pool state across cells).

On the queue path, `kernel_process` grant-checks `entry.cap_id`
(`queue/mod.rs:510`) - but the capability checked is the **queue-pair** cap,
minted `READ|WRITE`. So the per-entry check gates the **ring, not the resource**:
any cell holding a queue pair can transmit arbitrary Ethernet frames, read any
received frame, present to the display, and `OP_OPEN` any path.

A socket `ObjectKind` and steering grants are honestly deferred **(C)**; what is
not honest is presenting the per-entry grant check as gating the operation.

## 3. Structural defects found by the dependency graph

### 3.1 The on-wire ABI is written out twice, by hand (A - silent-corruption class)

| Item | Kernel | Cell |
|---|---|---|
| `SqEntry` / `CqEntry` / `QueueHeader` | `kernel/src/queue/mod.rs:139,156,212` | `librheo/src/sys.rs:674,688,698` |
| `QUEUE_ABI_VERSION` | `queue/mod.rs:133` | `sys.rs:711` |
| `SYS_*` | `abi.rs` (49) | `sys.rs` (28), `libc/src/sys.rs` (7) |
| `OP_*` | `queue/mod.rs` (12) | `sys.rs` (13) |

Struct bodies diff **byte-for-byte identical**; all shared constants agree
(28/28, 7/7). So this is **latent, not live** - but geometry drift is the only
kind that is caught (librheo reads the offsets at runtime and asserts the
version). **Field-meaning drift within the same size is not caught**, nor is a
`SYS_`/`OP_` number changing on one side - both produce wrong numbers with no
fault, which is exactly the failure mode the FP/SIMD switch bug already taught
this tree. Asymmetric guard: the kernel asserts
`size_of::<QueueHeader>() == 64`; **librheo does not**.

Fix: a `rheo-abi` leaf crate (`no_std`, zero deps, **no lang items** - the
`runtime/` model). Divergence becomes a compile error.

### 3.2 The object layer names concrete device drivers (A - `ARCHITECTURE.md` §5)

One 90-line `match` in `kernel/src/queue/mod.rs` contains both the disciplined
and the undisciplined approach:

```
:573  OP_OPEN        -> crate::svc::file_ops()...            <- bridge
:627  OP_GRAPH_SUBMIT-> crate::svc::graph_submit(..)          <- bridge
:636  OP_NET_TX      -> crate::hw::virtio_net::tx(..)         <- NAMES A DRIVER
:657  OP_GPU_PRESENT -> crate::hw::virtio_gpu::present(..)    <- NAMES A DRIVER
```

§5 puts "device drivers beyond queue/IOMMU/reset plumbing" permanently outside
the kernel. The remedy sits 20 lines above the defect: two more `svc` tables,
`NicOps { tx, rx, mac }` and `DisplayOps { present }`. **Cheapest §5 compliance
win in the tree**, and it composes rather than extends.

### 3.3 A hidden layering edge cargo cannot see (A)

`kernel/src/engine.rs:25` compiles a file out of librheo's source tree:

```rust
#[path = "../../librheo/src/tile/kernels.rs"]
mod tile_kernels;
```

The bottom layer, with a hard zero-dependency rule, reaches into a higher one.
It technically preserves "zero deps" but trades a *checked* dependency for an
*unchecked* one - nothing warns if librheo moves or edits that file, and it needs
`#[allow(dead_code)]` because most of it is unreachable. `bench_core.rs:60` does
the same. Fix: a `rheo-tile-kernels` leaf crate that all four consumers depend on
normally.

### 3.4 librheo's lang items force a posture split in another crate (A)

librheo emits three binary-level lang items and **none is behind any feature**:
`#[panic_handler]` (`librheo/src/lib.rs:62`), `#[global_allocator]`
(`mem.rs:81`), `_start` (`start.rs:23,32`) - and `mem`/`start` are in the
always-compiled spine. So **no librheo posture can link beside a kernel
binary**. `rheo-net` therefore had to make librheo optional, which drops seven
public modules.

Measured cost: **57** `cfg(feature = "hosted")` gates in `net/src`; a build
landmine documented in **two** `Cargo.toml` files (*never build `-p qemu-tests`
and `-p rheo-net` in one invocation*); 6+ forced-separate xtask invocations; and
a bifurcated public API.

**The counter-proof is already in the tree:** `runtime/` contains **zero** lang
items and is linked by both a kernel binary and a cell with no feature gymnastics
at all. librheo did not follow the model its own dependency set.

### 3.5 A fault-injection fixture ships in the library (A, small)

`VirtualLink` (`net/src/tcp.rs:1304-1361`) is `pub`, carries a `drop_next_data`
fault-injection flag, and there is **not one `cfg(test)` in the whole `net`
crate** - so a test loopback compiles into every posture, including the codec
posture that links into kernel binaries. Gate it behind a non-default `proof`
feature.

### 3.6 Smaller structural items

- **Three free module cycles.** `arch -> time`, `arch -> rng`, `arch -> svc` are
  **3 references total**, all at `arch/mod.rs:144,146,147` - `arch::init()` is a
  *boot sequencer* living in the arch namespace. Moving it to
  `kernel::boot::init()` deletes three cycles in a five-line change. **(B)**
- **The `arch` seam is not trait-shaped.** 83 flat re-exports, **zero traits**,
  against a doc that says "one trait per concern". But the naming already
  clusters them into 9 coherent concerns (`TrapContext` 15, `Paging` 13, `Timer`
  11, `Mmio` 8, `Interrupts` 7, `FpSimd` 5, `SignalFrame` 5, `Console` 4,
  `Hwrng` 3) with 79 of 83 fitting cleanly - so this is the *easiest* defect to
  close; only the boilerplate is missing. **(B)**
- **The IOMMU sits outside its declared seam.** `TARGET-ARCHITECTURES.md` §4
  lists IOMMU as an arch-trait area, but `hw/iommu.rs` (VT-d) and `hw/smmuv3.rs`
  are compiled on all three ISAs and the per-ISA choice is made by the
  **consumer, in a test** (`tests/src/iommu.rs:26,28`). 599 lines reachable only
  from one test kernel. It is the only §4 area that is implemented and
  misplaced. **(A, small)**
- **Per-ISA leakage: 9 sites, one file.** All in `kernel/src/user_progs.rs`; 3
  carry a written justification, **6 do not**, and no doc claims the exemption.
  The reason is good and identical for all nine - a U-mode syscall instruction
  and a cycle counter *are* the ISA, and these programs cannot reach `arch/`
  because kernel `.text` is not mapped in a cell. Fix is a paragraph, not a
  refactor. **(C once written; a rule violation until then.)**
- **Two `install_script` implementations in the kernel** with the same doc
  comment (`input.rs:96`, `pty.rs:20-34`), each with its own `static mut SOURCE`
  and `Source::Script` variant, consumers split between them. **(B)**
- **Object 5 implemented twice, and the shipped path ignores the memory kind.**
  `mm/grant.rs` is the typed implementation but is used **only by tests**; the
  path a cell reaches (`SYS_GRANT` -> `grant_create`, `user.rs:470-526`) uses a
  different struct and commits through the **DDR** allocator - `frames_pmem` is
  never consulted. So "PMEM real where a QEMU nvdimm is exposed" is true of the
  test-only type and **false of `SYS_GRANT`**: a cell asking for `Pmem` silently
  gets DDR with no printed reason, which is exactly what `ENGINEERING.md` §7
  forbids. **(A)**
- **`SYS_GRANT_SHARE` hardcodes `cur ^ 1`** (`user.rs:607`) while `SYS_CONNECT`
  gained a slot argument for fan-out - so zero-copy sharing does not compose with
  a multi-client service, contradicting a documented claim. **(B)**
- **`Rights<MASK>` is decorative.** `Cap::from_handle` is an unrestricted
  `pub const fn` over a raw `u64` (`runtime/src/rights.rs:56-61`), and
  `attenuate` returns the **same handle** with a different phantom type - and
  since there is no derive syscall (§2.1), a narrowed capability still carries a
  handle the kernel accepts for a wider right. The `SubsetOf` machinery is
  correct; nothing ties it to the kernel's stored rights. **(A)**
- **Object 10 (event streams) has no kernel emitters** - six event kinds with
  zero emit sites, one global ring rather than the per-cell bounded rings §4.10
  describes. Object 8 (lease) is acquired once and dropped. **(B)**

## 4. Compatibility gap: unmodified Claude Code

The binding goal. **Claude Code is no longer Node**: since v2.1.113 it ships as a
Bun-compiled native ELF (JavaScriptCore), ~100 MB, dynamically-linked glibc,
AVX2 baseline; npm only bootstraps it. Two immediate consequences: **Bun has no
riscv64 build**, so the three-ISA invariant cannot hold for this goal
(x86-64 + aarch64, riscv64 skip-with-reason); and `io_uring` is no longer a
blocker (Bun >= 1.0.16 uses epoll).

Three blockers are **design decisions**, not unfinished work:

1. **W^X is structural.** `MapPerm` has three variants and no RWX
   (`arch/mod.rs:44-51`), and `mmap(PROT_READ|WRITE|EXEC)` **returns success
   while silently dropping EXEC** (`linux/mem.rs:42-50`). JSC maps its JIT pool
   RWX on Linux, so it would fault jumping into generated code with **no
   diagnostic**. The `mprotect` RW->RX flip path does work, so a flipping JIT is
   viable where an RWX one is not. Either add `UserRwx` (a doctrine change
   needing a §6 pass) or run with the JIT off via an environment variable.
2. **No VMA list at all.** `mmap` is a forward bump cursor (`mem.rs:106-112`), so
   nothing detects that a large reservation spans `LINUX_INTERP_BASE` where ld.so
   and libc live. Silent corruption, not an error. The user VA ceiling is
   **256 GiB on every ISA** (`user.rs:62`, Sv39's user half applied uniformly).
3. **No idle state and no resumable page fault.** §2.4 above; plus
   `on_user_trap` maps every user fault to a signal or termination, so nothing is
   demand-paged and nothing grows on fault - against a ~100 MB binary that is
   **eagerly copied page-by-page into private frames**.

**The stub class is the practical hazard**, because a stub that reports success
puts the failure far from the cause. The base of the ladder has now been cleared;
what is left is named precisely.

**Fixed** (each with a proof observed to fail without it - see
docs/LINUX-COMPAT.md for the semantics and the fixtures):

| Was | Now |
|---|---|
| **DNS**: no `/etc/resolv.conf` anywhere, so glibc fell back to `127.0.0.1:53`, classified loopback, routed to a queue nothing listens on - and `sendto` **reported success anyway** | `/etc/{nsswitch.conf,hosts,resolv.conf}` seeded into the `linuxnet`-class ramfs (nameserver `10.0.2.3`, non-loopback); a loopback datagram to a port with **no bound endpoint** returns `-ECONNREFUSED`. `resolve.c` asserts the refusal + a deterministic `/etc/hosts` answer, and reports a **live** `getaddrinfo` (resolves on all three ISAs) |
| **`futex` timeouts treated as infinite**, so `pthread_cond_timedwait` hung forever | the timespec in arg 3 is read (relative for WAIT, absolute for WAIT_BITSET, CLOCK_REALTIME under `FUTEX_CLOCK_REALTIME`), compared in the **cell's own clock domain**, parked on through the timer arbiter's new `FutexWait` slot; elapsed → `-ETIMEDOUT`. An unsatisfiable wait reports `-EAGAIN` + one console line instead of 0 ("you were woken"). `condwait.c` hangs to the 120 s timeout without the fix |
| **`fcntl` unknown commands all return 0**; `F_SETFL` discards `O_NONBLOCK`; `F_GETFL` returns a literal `O_RDWR`; `FD_CLOEXEC` untracked so `execve` kept every fd | locking → `-ENOLCK`, anything else unimplemented → `-EINVAL`; `O_NONBLOCK` **honoured** (would-block → `-EAGAIN`), `O_APPEND`/`O_ASYNC` **refused**; `F_GETFL` reports the real access mode; `FD_CLOEXEC` tracked and honoured by `execve`. `fcntlx.c` asserts all four |
| **stdin `read` returns 0 = EOF** on an empty FIFO | still 0 when blocking (no cell may park on the console - documented), but `-EAGAIN` once `O_NONBLOCK` is set, which is the answer a caller can act on |
| **Limits**: 128 MiB pool / 96 MiB per cell / 1 MiB stack, sized for a few-hundred-KiB fixture | 512 MiB / 384 MiB / 8 MiB, with `RLIMIT_STACK` derived from the one stack constant. A **limit raise, not a design change** - demand paging is still the real answer (blocker 3 above) |
| `op_tcp_send` never pumped RX, so ACKs never reached the state machine, the send queue filled and `write` returned 0 → EAGAIN forever: **any body larger than the send window deadlocked** | the send path drains the NIC first. **Reasoned + code-reviewed, unproven** - it needs the deterministic TCP peer the data path needs, and none was invented |

**Still open, and now measured rather than suspected:**

- **`poll`/`epoll_wait`** are worse than "ignore fd kind": `poll` does not consult
  readiness **at all** - every open fd is reported ready for whatever was asked,
  a closed one `POLLNVAL`, and the timeout is ignored. Two things depend on that
  accident: it is what lets glibc's resolver fall through to its **blocking**
  `recvfrom` (so DNS works *because* of the bug), and it is why creation-time
  `O_NONBLOCK`/`SOCK_NONBLOCK` is still dropped - honouring it would turn every
  non-blocking program into a spin that fails. A `poll` that computes readiness
  **and waits for its timeout** plus creation-time `O_NONBLOCK` must land
  **together**. This is the next rung.
- `nanosleep` returns immediately; `readlinkat` is hardcoded `-ENOENT`;
  cross-process `kill` does not exist and `kill(0)`/`kill(-1)` **silently
  self-target** - which matters because subprocess management is the
  application's core function.

One hazard learned while bounding the futex timeout, worth recording because it is
a *new* instance of §12's rule rather than the old one: a cell-supplied argument
that is a pointer **only for some commands** must be validated command-aware.
Validating futex arg 3 unconditionally refused a legitimate `FUTEX_WAKE` with
`-EFAULT` (it is a plain *count* there, and real callers reach the syscall with
that register simply left dirty), which silently stopped every waiter from being
woken. It surfaced as rayon-threaded `sort` producing **no output at all** on
ARM64 while the other two ISAs passed - a one-ISA symptom of a portable mistake.

A 13-rung ladder from here to the goal is in the task list, each rung a real
binary with an assertable outcome. Two tooling facts shape it: rungs past a
"hello" will not fit the 120 s test timeout under TCG, and nothing in the tree
uses KVM - a KVM-accelerated lane is a prerequisite, not an optimisation.

## 5. Duplication that has earned a framework

Measured, not estimated.

| Pattern | Duplication | Framework | Effort / risk |
|---|---|---|---|
| **Test-kernel boilerplate** | **~2,684 removable lines** across 26 kernels; 130 boilerplate : 28 unique per kernel (4.5:1; 11:1 for pure-net); the console `FileOps` 8-stub block copied **22 times verbatim**; 14 launch blocks differing by **2 lines**; **10** independent `macro_rules!` for the same per-ISA `include_bytes!`, two byte-identical *and on the same line number* | `console_personality::ops()` + `harness::run_elf_cell` + `elf!`/`heap!` macros | M / **LOW** (test-only; failures are loud) |
| **The virtio trio** | **~860 of 2,391 lines (36%)**; 48 constants in >1 driver; `PciXport` 3x (net vs gpu diff to **zero lines**); 5 ring structs 3x; the reset sequence character-identical in **all six** places | `hw/virtio/`: `trait VirtioTransport`, `VirtQueue<N>` with `submit_chain(&[Seg])`, `negotiate()` | L / **S(gpu) → M(blk) → L(net)** |
| **Fixed-slot registries** | 22 registries, ~1,080 lines; **12 fit one generic** (7 are literally the same 8 lines) | `Registry<T: Slot, N>` - **without** generations (the only site needing them stays bespoke) | M / MEDIUM (live kernel state; quiet failures) |
| **The `svc` bridge** | two structurally identical 17-line static/setter/getter triples; **31** hand-written null-check call sites in two idioms | `Bridge<T>` + `NicOps`/`DisplayOps` (+ `PersonalityOps` later) | S / LOW |
| **The proof harness** | `VirtualLink` defined once (good) but the **pump loop exists in 4 hand-written copies** (~131 lines) and there are **six** notions of "advance a controlled clock" | `net::proof` behind a non-default feature: `LogicalClock`, `VirtualLink`, one `pump()` | S-M / LOW |
| **The honesty pattern** | re-invented three times (`input`, `net_rx`, `ktimer`) - same accessor bodies, same `SAFETY` comment text; skip-with-reason is free-form prose at **19** sites with **three** different markers | `Validated<T>` + `Evidence`; a typed `Skip` enum | M / **MEDIUM** on `Evidence` (the counters *are* the proofs), LOW on `Skip` |

### The meta-observation

Four places in this tree contain the abstraction the code needed, applied once
and then not reused:

- `tests/src/vfs_personality.rs:49` - the shared-`FileOps` pattern, used by 11
  kernels while 22 others hand-roll a console variant.
- `tests/src/librheoproc.rs:124` - `run_cell(image, spawn_cap, script)`, exactly
  the missing harness entry point, scoped to one file.
- `runtime/` - a library with **zero lang items**, linked by both a kernel and a
  cell, which is precisely what librheo needed to be.
- `queue/mod.rs:573` - `svc::file_ops()`, the §5-compliant bridge, sitting 20
  lines above four opcodes that name drivers directly.

The gap is not design judgement. It is that a good pattern gets invented, used
once, and then **copied instead of imported**. The sharpest evidence is the ten
independently-written macros for one per-ISA path, two of which are byte-identical
*and land on the same line number*. Most of §5 is finishing a decision the tree
already made.

## 6. Order of work

Chosen by (correctness at risk) first, then (leverage ÷ risk).

**Now - correctness.** §2.4 the scheduler idle state (it is both the enforcement
defect and the compatibility keystone); §2.1-2.3 the capability surface, complete
revocation, per-child tables; §2.5 a system-wide admission ledger; §2.6 the
ambient-authority sweep.

**First week - four Small, near-zero-risk items closing three structural
defects.** `arch::init -> kernel::boot::init` (5 lines, deletes 3 cycles);
`rheo-abi` (removes the only silent-corruption duplication); `svc::Bridge<T>` +
`NicOps`/`DisplayOps` (closes the §5 defect); write the two `cfg` exemptions.

**Then - leverage.** The test harness (largest line win, lowest risk, and it
makes every later refactor cheaper to prove); gate `VirtualLink`;
`rheo-tile-kernels`; the virtio core staged gpu → blk → net; the librheo lang-item
split (deletes `net`'s posture split permanently); `Registry`; `Skip`; the `arch`
traits; `Validated`/`Evidence`.

**Throughout.** Every step additive per `ENGINEERING.md` §8: the pre-existing
proofs must pass **unedited**. If a proof needed editing, the step was not
additive - find out why.
