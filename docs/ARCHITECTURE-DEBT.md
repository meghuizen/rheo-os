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
| **§2.4 Kernel-blocking waits did not reschedule**, and `reschedule` **panicked** when nothing was runnable - so a process waiting for the outside world could not be expressed. | Fixed. `kernel/src/idle.rs` is a **scheduler idle state** composing `ktimer` + `net_rx` + `input` (no new object, no new verb): `SYS_ARM_TIMER`/`SYS_WAIT_INPUT`/`SYS_WAIT_NET` and the Linux `nanosleep`/`poll`/`epoll_wait`/console read all **register** their condition and return to the run loop, and the scheduler halts only when nothing is runnable. A genuine no-wake-source state prints which cell/pid is blocked on what and ends the run with `DEADLOCK_EXIT`. Proven by `schedidle` (two cells, an unfakeable ordering witness `bSSSSSSSSB`) and `linuxpoll`, on all three ISAs; both observed failing without the fix. |
| **§2.4 `poll` did not compute readiness at all** - every open fd reported ready, timeout ignored; `epoll_wait` never waited; `nanosleep` returned immediately; blocking stdin answered 0 = EOF; creation-time `O_NONBLOCK` dropped. | Fixed, all in one slice because DNS depended on the combination. `poll`/`ppoll`/`epoll_wait` compute real per-`FdKind` readiness and honour their timeout by parking; `svc::SocketOps` gained the missing `tcp_pending` (its absence *was* the hardcoded "remote TCP is always readable"); `nanosleep` is real in the cell's clock domain; stdin blocks; creation-time `O_NONBLOCK`/`SOCK_NONBLOCK` is honoured. |
| **§2.5 Admission could not refuse over-commit** - sixteen cells at 90% each all succeeded, and a **second** global controller existed behind the legacy `SYS_RESERVE`, which discarded its `Reservation` and leaked monotonically. | Fixed. One machine-wide ledger (`sched::system()`); a reservation must fit its cell **and** the machine, and a refusal leaves both unchanged. The legacy verb shares that ledger instead of keeping a private one, and its no-release cumulative-probe nature is now documented at the call site rather than being a silent leak. `schedidle` asserts the hand-computed oracle: 900,000 ppm admitted, a second 900,000 refused as over-commit while each *cell's* own controller would have accepted it. |

| **§3.1 The on-wire ABI was written out twice by hand** - 28 `SYS_*`, 12 `OP_*` and 12 `repr(C)` structs restated in `librheo/src/sys.rs` under a "keep in sync" comment. Values agreed, so it was latent; a field-meaning change within the same size would have been wrong numbers with no fault. | Fixed. A **`rheo-abi`** leaf crate (`no_std`, zero deps, **no lang items** - the `runtime/` model) is the single definition; `kernel::abi`, `kernel::queue`, `librheo::sys` and `libc::sys` all `pub use` it, so no call site changed and divergence is now a compile error. **943 lines of hand-kept duplicate deleted.** The asymmetric guard is gone too: `size_of::<QueueHeader>() == 64` is asserted once, for both sides. |
| **§3.2 The object layer named concrete device drivers** - `queue/mod.rs` reached `hw::virtio_net::{tx,rx,mac}` and `hw::virtio_gpu::present` directly, twenty lines below the `svc::file_ops()` bridge that does it correctly. `ARCHITECTURE.md` §5 puts drivers permanently outside the kernel. | Fixed. `svc::NicOps`/`svc::DisplayOps` behind a new **`svc::Bridge<T>`**, registered by the driver's own `install()` - where the device is known to exist, so a kernel with no netdev answers `STATUS_IO` honestly instead of calling into a driver that is not there. `queue` names no driver. A driver **cell** installs into the same slot later with nothing above it changing. |
| **§5 The `svc` bridge pattern was hand-written per table** - two structurally identical static/setter/getter triples, about to become four. | Fixed. One `Bridge<T>`; `install` and `get` are `unsafe` with the boot-time-only invariant named, and the four safe accessors are where it is discharged. |
| **§3.6 `arch::init()` was a boot sequencer in the arch namespace**, reaching up into `crate::{time, hw, rng, svc}` - three module cycles from three lines, none of them per-ISA. | Fixed. `kernel::boot::init()` sequences the portable half; `arch::init()` does only console + vectors + kernel page tables and names nothing above it. 61 call sites renamed mechanically (no assertion changed). Only the three **free** cycles closed - the per-ISA modules still reach `hw`/`input`/`user`/`net_rx`, which carry real coupling and need a registration seam; see §3.6. |
| **§3.6 Per-ISA leakage outside `arch/` had no written exemption** - 9 sites in `user_progs.rs`, 6 with no justification, and no doc claimed the rule was relaxed. | Fixed by writing it down: `TARGET-ARCHITECTURES.md` §4.1 states both exemptions and their bounds, with the counts (138 of 146 `tests/src` sites are per-target *file paths*), and names `iommu.rs`'s five sites as a **defect, not an exemption**. |

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

### 2.4 Kernel-blocking waits do not reschedule - **CLOSED** (see §1)

Was the keystone: simultaneously the enforcement defect and rung 1 of the
compatibility ladder (§4). `kernel/src/idle.rs` now holds the scheduler idle state
the entry called for, and `IO.md`/`CONCURRENCY.md`'s "blocking does not exist below
the library level" is true of the kernel - with the cooperative scope it does *not*
cover stated in both docs.

### 2.5 Admission cannot refuse over-commit - **CLOSED** (see §1)

`sched::system()` is the machine-wide ledger; the legacy `SYS_RESERVE`'s second
controller is gone. What is *not* closed and is now named at the call site: that
verb still has no release, so it is a cumulative probe rather than a managed
reservation (`SYS_RESERVE_ADMIT`/`RELEASE` is the real, capability-gated surface).
Also still open, and unchanged by this: an admitted reservation is not yet
**enforced** at runtime - the scheduler is single-CPU cooperative, so admission is
math that refuses cleanly, not a guarantee anything schedules (task #27).

### 2.6 Ambient authority, and a grant check that gated less than it claimed (A + C - *partly closed*)

`svc::handle` (`svc.rs:49-169`) is reached for any native syscall the main match
does not claim and performs **no capability check on any of its 18 verbs** -
including the whole file surface, `SYS_RANDOM`, `SYS_LEASE`, and `SYS_MEMINFO`
(which leaks global pool state across cells).

On the queue path, `kernel_process` grant-checks `entry.cap_id`
(`queue/mod.rs:510`) - but the capability checked is the **queue-pair** cap,
minted `READ|WRITE`. So the per-entry check gates the **ring, not the resource**:
any cell holding a queue pair can transmit arbitrary Ethernet frames, read any
received frame, present to the display, and `OP_OPEN` any path.

A socket `ObjectKind` and steering grants are honestly deferred **(C)**; what was
not honest is presenting the per-entry grant check as gating the operation.

**Closed, the dishonest part.** Two things were wrong beyond the deferral, and
both are fixed:

1. *The check discarded the object it resolved.* `Ok(_object) => run_opcode(..)` -
   so `entry.cap_id`, which the **cell** chooses, only had to name *some* live
   capability with the right bit set. A `MemoryGrant` id worked exactly as well as
   the queue's. It now must name a **`QueuePair`**; a wrong-kind capability
   completes `STATUS_DENIED`. Proven in `queue_pipeline` from a real cap table,
   with a control showing the queue's own capability still works - and observed
   failing with the guard removed (`left: 0, right: 2`, i.e. the grant cap was
   accepted).
2. *The claim.* `kernel_process`'s doc now states exactly what the check
   establishes (a live capability in **this** cell's table, the opcode's right,
   a current epoch, and the QueuePair kind) and what it does **not** (which
   resource the opcode reaches - a cell with a queue can still TX an arbitrary
   frame, present, or `OP_OPEN` any path the registered `FileOps` will open),
   naming what would close it.

**Still open: the 18 unchecked `svc` verbs**, and it is coupled to §2.1 rather
than independent - which is why it is not fixed here. Gating `SYS_UPTIME` /
`SYS_RANDOM` / `SYS_LEASE` / `SYS_MEMINFO` on a capability requires Clock,
Entropy, Lease and EventStream to *be* capability kinds a cell can hold, and
`ObjectKind` has none of them. Adding the kinds is admissible (those objects are
already in ARCHITECTURE.md 3; making them addressable *is* §2.1, not a new
object), but retrofitting the gate would edit the setup of ~30 pre-existing
proofs, so it must land **with** §2.1's mint/derive surface, not before it.

## 3. Structural defects found by the dependency graph

### 3.1 The on-wire ABI was written out twice, by hand - **CLOSED** (see §1)

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

Fixed: the `rheo-abi` leaf crate (`no_std`, zero deps, **no lang items** - the
`runtime/` model) is now the single definition and all four consumers re-export
it. Divergence is a compile error. The table above is kept as the record of what
the duplication *was*.

### 3.2 The object layer named concrete device drivers - **CLOSED** (see §1)

One 90-line `match` in `kernel/src/queue/mod.rs` contains both the disciplined
and the undisciplined approach:

```
:573  OP_OPEN        -> crate::svc::file_ops()...            <- bridge
:627  OP_GRAPH_SUBMIT-> crate::svc::graph_submit(..)          <- bridge
:636  OP_NET_TX      -> crate::hw::virtio_net::tx(..)         <- NAMES A DRIVER
:657  OP_GPU_PRESENT -> crate::hw::virtio_gpu::present(..)    <- NAMES A DRIVER
```

§5 puts "device drivers beyond queue/IOMMU/reset plumbing" permanently outside
the kernel. The remedy sat 20 lines above the defect, and is what landed: two
more `svc` tables, `NicOps { tx, rx, mac }` and `DisplayOps { present }`, behind
the new `Bridge<T>`. It composed rather than extended - the virtio drivers did
not move, they register themselves.

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

### 3.5 A fault-injection fixture shipped in the library - **CLOSED** (see §1)

`VirtualLink` (`net/src/tcp.rs:1304-1361`) is `pub`, carries a `drop_next_data`
fault-injection flag, and there is **not one `cfg(test)` in the whole `net`
crate** - so a test loopback compiles into every posture, including the codec
posture that links into kernel binaries. Gate it behind a non-default `proof`
feature.

### 3.6 Smaller structural items

- ~~**Three free module cycles.**~~ **CLOSED.** `arch -> time`, `arch -> rng`,
  `arch -> svc` were 3 references total, all in `arch::init()` - a *boot
  sequencer* living in the arch namespace. `kernel::boot::init()` now sequences
  the portable half and `arch::init()` names nothing above it. The five-line
  estimate was right about the logic and wrong about the blast radius: 61 call
  sites had to be renamed, mechanically and with no assertion touched.

  Scope, stated so the claim is not read wider than it is: these three were the
  **free** cycles - a sequencer that happened to live in the wrong namespace, with
  no coupling behind it. The per-ISA modules still reference `crate::hw` (20
  sites), `crate::input` (5), `crate::user` (4) and `crate::net_rx` (2), and those
  edges are **not** free: an interrupt handler written in `arch` genuinely has to
  deliver the byte or the frame to the portable sink, and paging genuinely needs
  `mm`. Removing them means inverting the direction with a registration seam (the
  `Bridge<T>` shape, or a per-ISA `IrqSink`), which is real design work and a
  separate item - not part of this one.
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
- ~~**Per-ISA leakage: 9 sites, one file.**~~ **CLOSED (C).** The paragraph is
  written: `TARGET-ARCHITECTURES.md` §4.1 states the two exemptions and their
  exact bounds - inline asm for the syscall instruction and counter read in code
  that *runs in U-mode* (which cannot reach `arch/`, because kernel `.text` is
  not mapped in a cell), and per-target file paths in test kernels. Writing it
  down also separated a real defect from the noise: `iommu.rs`'s five sites are
  named there as a violation, not an exemption.
- ~~**Two `install_script` implementations in the kernel**~~ **CLOSED (partly;
  the rest is not de-duplication).** Both had the same doc comment
  (`input.rs:96`, `pty.rs:20-34`), their own `static mut SOURCE`, their own
  `Source::Script` cursor, and consumers split between them (three kernels on
  `input`, one on `pty`). `input` now owns the single scripted source and exposes
  `script_next_byte`/`scripted`; `pty::install_script` forwards to it.
  **Deliberately not merged:** the two **live** paths differ in behaviour, not
  spelling - `input`'s serial arm feeds the interrupt-driven RX ring that
  `SYS_WAIT_INPUT` parks on, while `pty`'s polls and blocks inside the cooked
  line read. Unifying those is a console-path change needing its own proof, and
  calling it de-duplication would have hidden that.
- ~~**Object 5 implemented twice, and the shipped path ignores the memory
  kind.**~~ **CLOSED.** `mm/grant.rs` is the typed implementation but was used
  **only by tests**; the path a cell reaches (`SYS_GRANT` -> `grant_create`) used
  a different struct and committed through the **DDR** allocator - `frames_pmem`
  was never consulted. So "PMEM real where a QEMU nvdimm is exposed" was true of
  the test-only type and **false of `SYS_GRANT`**.

  Fixed: `commit_range_from(.., Backing)` takes the pool explicitly, `grant_commit`
  derives it from the grant's recorded kind, and `SYS_DECOMMIT` routes each frame
  back to the pool it came from (getting *that* wrong would have been a kernel
  panic from a cell, since `frames::free` asserts on a non-pool address). Where no
  nvdimm exists the fallback to DDR is **printed once per kind**, as are the
  emulated-as-DDR kinds (Hbm/Cxl/Remote) - §7 wants the fallback visible, not
  merely documented. The librheo doc that claimed `Pmem` was DDR-backed like the
  others is corrected.

  Proven by `pmem`, which now runs a **cell** (`librheo-pmem`) through the real
  `SYS_GRANT`/`SYS_COMMIT` path and asserts kernel-side, on evidence the cell
  cannot influence, that the kernel's own pmem free count fell by the pages the
  cell committed: **4 frames from the nvdimm pool** on x86-64, and the
  printed-reason DDR fallback with **0** pmem frames consumed on arm/riscv.
  Observed failing with the fix reverted: *"SYS_GRANT(Pmem) drew 0 pmem frames,
  expected at least 4 - the typed kind never reached the allocator"*.
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
3. **No resumable page fault.** (The missing *idle state* was the other half of
   this and is now closed - §2.4.) `on_user_trap` still maps every user fault to a
   signal or termination, so nothing is demand-paged and nothing grows on fault -
   against a ~100 MB binary that is **eagerly copied page-by-page into private
   frames**.

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
| **`poll` never consulted readiness**, `epoll_wait` never waited, `nanosleep` returned immediately, blocking stdin answered 0 = EOF, creation-time `O_NONBLOCK` was dropped - and the resolver worked *because* of the first and last of those | all fixed together (they cannot be separated - see docs/LINUX-COMPAT.md's `poll` row). `pollx.c` asserts an empty pipe is NOT readable, both timeouts elapse on the program's own clock, an indefinite `poll` is woken by a forked peer, a 40 ms `nanosleep` sleeps, and `pipe2(O_NONBLOCK)` reports EAGAIN; `linuxnet`'s resolver still resolves. Both observed failing without the fix |
| **`reschedule` panicked** when nothing was runnable, so "waiting for the network" was not an expressible state | a scheduler idle state (`kernel/src/idle.rs`); an unsatisfiable wait is reported, naming the blocked pid and its wait, and ends the run with `DEADLOCK_EXIT`. `polldead.c` reaches that branch deliberately |

**Still open, and now measured rather than suspected:**

- ~~`readlinkat` is hardcoded `-ENOENT`~~ **CLOSED.** `/proc/self/exe` (and
  `/proc/<own pid>/exe`) resolves to the path `execve` recorded; a real file that
  is not a link answers `-EINVAL` and an absent path `-ENOENT`. A cell the test
  kernel loaded directly has no recorded path and gets `-ENOENT` rather than an
  invented one. `killx.c` asserts all three; observed failing reverted.
- **Cross-process `kill`: written, then reverted unshipped. Still open.** The
  implementation was straightforward and is worth restating so it is not
  redesigned from scratch: look the pid up in the process table
  (`cell_of_pid`), `post` the signal pending on the target's main context, and
  deliver it from `reschedule` immediately after switching in - a frame rewrite
  needs the target's own context with its address space active, so that is the
  only place it can happen. `kill(pid, 0)` becomes a real existence probe;
  `kill(0)` fans out to every live process (there is no `setpgid`, so every
  process genuinely *is* in the initial group); `kill(-1)` fans out **excluding
  the top of the tree**, standing in for init; a negative pid other than -1 names
  a group that does not exist and is refused rather than redirected to the caller.

  It behaved correctly on riscv64 - including the discriminating case,
  `kill(-1)` sparing init, observed failing as *"kill(-1) self-targeted init"*
  when reverted - and then **failed on x86-64**, so it was reverted rather than
  shipped broken on one ISA.

  The x86-64 cause is now known, because the `readlinkat` half of the same slice
  failed there the same way and was root-caused: **glibc on x86-64 issues the
  legacy `readlink` (89), not `readlinkat` (267)**, and the asm-generic table has
  only `readlinkat` - so the dispatch arm matched on two ISAs and not the third.
  Fixed with the sentinel pattern the tree already uses for `ACCESS` (a real
  number on x86-64, an unreachable `u64::MAX - n` on asm-generic), and now proven
  on all three ISAs. Whether `kill` has a second, independent x86-64 problem or
  the same class of missing legacy arm is the first thing to check when it is
  picked up again - `kill` is 62 on x86-64 and 129 on asm-generic, both already
  dispatched, so it is not that number.

  The general hazard, worth its own line: **a syscall that exists under two
  numbers is a portability trap that only one ISA exercises.** x86-64's legacy
  arms (`access`, `pipe`, `dup2`, `readlink`, ...) are issued by glibc *in
  preference to* the `*at` forms, so an implementation written against the
  asm-generic table passes on aarch64 and riscv64 and silently does nothing on the
  ISA that matters most for this goal.

  Two notes on method, both earned here:
  - The first three `kill` phases written (self probe, absent pid, unknown group)
    all passed **with the fix reverted** - the old stub happened to give the same
    three answers. A proof that does not discriminate is not a proof, and the
    only way to find that out is to revert and re-run. That is what produced the
    `kill(-1)`-spares-init phase, which does discriminate.
  - Two mechanisms could let a child wait while the parent signals it, and both
    turned out to be broken - the next two entries. Neither was known before
    trying to write this proof.
- **A forked child exits instead of waiting - cause NOT yet identified (B).**
  *Retracted claim, kept as a correction.* This entry first asserted that pipe
  reader/writer counts are not refcounted across `fork`, which would make a child
  blocked on an inherited pipe satisfiable via `writers(idx) == 0` and its `read`
  return 0 = EOF. **That is wrong**: `linux::dup_state` (called by
  `proc::fork`) does call `fds.inherit_pipe_ends()`, `pipe::add_end` is real, and
  `dup` bumps the same counts via `bump_if_pipe`. The refcounting is implemented.

  What was actually **observed**, and still is unexplained: in a fixture where the
  parent forks and the child is meant to wait to be signalled, the child reached
  `Zombie` with `wstatus 0x400` (exit 4 = "no signal arrived") *before* the parent
  reached its `kill`. That held across three child designs - a read on an inherited
  pipe, a read on a pipe the child created itself after the fork, and a bounded
  `sched_yield` loop. The third is explained by the next entry; the first two are
  not.

  Recorded this way on purpose. The inference was published one step ahead of the
  evidence, which is the same mistake `ENGINEERING.md` §1 exists to catch when
  someone else makes it - a plausible mechanism is not a diagnosis. Whoever picks
  this up should start from the observation, not from the retracted cause.
- **`sched_yield` does not yield across cells (B).** It reschedules among a cell's
  own contexts (L4 threads), so a child looping `sched_yield()` runs to completion
  before the parent is scheduled at all. A cooperative cross-cell scheduler needs
  `sched_yield` to reach `proc::reschedule`, the way the native `SYS_YIELD` does
  (docs/NETSTACK.md 17). (`poll`/`epoll_wait`/`nanosleep`,
  blocking stdin and creation-time `O_NONBLOCK` were this rung and are now closed -
  see §1. The DNS dependency turned out to be exactly as described: the resolver
  needed a `poll` that *waits*, because honouring `SOCK_NONBLOCK` removes the
  blocking `recvfrom` it used to rely on.)
- **`poll`/`epoll` scope that remains:** POLLERR/POLLHUP/POLLPRI are not reported,
  a poll set larger than 64 descriptors keeps the non-blocking probe, epoll stays
  level-triggered with no nesting, and a *thread*-level block still parks the whole
  cell rather than only the calling context (a multi-threaded process whose one
  thread `poll`s does not run its siblings meanwhile - the futex path does that
  correctly, this one does not yet).

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
| ~~**Test-kernel boilerplate**~~ **DONE (first pass)** | **~2,684 removable lines** across 26 kernels; the console `FileOps` 8-stub block copied **22 times verbatim**; 14 launch blocks differing by **2 lines**; **10** independent `macro_rules!` for the same per-ISA `include_bytes!` | `console_personality` (two honest variants) + `harness::run_elf_cell`/`run_linux_cell` + `fixture::cell!`/`linux!`/`linux_cargo!`. **-1,533 net lines** across 23 files, 22 kernels converted, every assertion unchanged. Remaining: the `heap!` macro and the `.user`-window kernels (`bench_core`/`isolation_hw`/`lsh`/`schedidle`/`security`/`shell_smoke`), whose launches are genuinely different | M / **LOW** (test-only; failures are loud) |
| **The virtio trio** - **NEXT** | **~860 of 2,424 lines (36%)** (net 921 / gpu 868 / blk 635, re-counted); 48 constants in >1 driver; `PciXport` net-vs-gpu re-diffed to **3 lines** - one field, `notify_off: [u32; 2]` vs `u32`, so it is one type parameterised by queue count (the register's earlier "zero lines" was optimistic, the substance holds); 5 ring structs 3x; the reset sequence character-identical in **all six** places | `hw/virtio/`: `trait VirtioTransport`, `VirtQueue<N>` with `submit_chain(&[Seg])`, `negotiate()` | L / **S(gpu) → M(blk) → L(net)**. Deliberately left for its own slice: it is live DMA driver code on three ISAs, so a half-converted trio shipped mid-slice is worse than a staged one |
| **Fixed-slot registries** | 22 registries, ~1,080 lines; **12 fit one generic** (7 are literally the same 8 lines) | `Registry<T: Slot, N>` - **without** generations (the only site needing them stays bespoke) | M / MEDIUM (live kernel state; quiet failures) |
| ~~**The `svc` bridge**~~ **DONE** | two structurally identical 17-line static/setter/getter triples; **31** hand-written null-check call sites in two idioms | `Bridge<T>` + `NicOps`/`DisplayOps` landed (`PersonalityOps` still later) | S / LOW |
| **The proof harness** - *partly done* | `VirtualLink` defined once (good) but the **pump loop exists in 4 hand-written copies** (~131 lines) and there are **six** notions of "advance a controlled clock" | `VirtualLink` is now behind the non-default `proof` feature (§3.5). Still to do: move it into a `net::proof` module with `LogicalClock` and one `pump()` | S-M / LOW |
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

**Now - correctness.** ~~§2.4 the scheduler idle state~~ and ~~§2.5 a system-wide
admission ledger~~ are **closed** (§1). Next: §2.1-2.3 the capability surface,
complete revocation, per-child tables; §2.6 the ambient-authority sweep.

~~**First week - four Small, near-zero-risk items closing three structural
defects.**~~ **DONE** (all four; see §1): `kernel::boot::init` deletes the three
cycles, `rheo-abi` removes the silent-corruption duplication (-943 lines),
`svc::Bridge<T>` + `NicOps`/`DisplayOps` closes the §3.2 defect, and
`TARGET-ARCHITECTURES.md` §4.1 states the `cfg` exemptions.

**Then - leverage.** ~~The test harness~~ **first pass DONE** (see §5: -1,533
lines, 22 kernels, no assertion touched); next: gate `VirtualLink`;
`rheo-tile-kernels`; the virtio core staged gpu → blk → net; the librheo lang-item
split (deletes `net`'s posture split permanently); `Registry`; `Skip`; the `arch`
traits; `Validated`/`Evidence`.

**Throughout.** Every step additive per `ENGINEERING.md` §8: the pre-existing
proofs must pass **unedited**. If a proof needed editing, the step was not
additive - find out why.
