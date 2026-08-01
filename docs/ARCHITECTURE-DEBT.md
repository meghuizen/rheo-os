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

### 2.1 The capability model has no userspace surface - **CLOSED for derive / revoke / inspect / drop**

`ARCHITECTURE.md` §3 named *mint / delegate / revoke* from the first draft, and
none of the 49 syscalls was any of them. `revoke_epoch`, `derive_subset` and
`delegate` had **zero production callers** - every caller was a test - so object
2's claim to be "simultaneously the security model, the audit log, and the
metering system" rested on a primitive nothing but a test had ever called, and
`abi.rs`'s promise that "an epoch revoke kills the peer's copy too" was
unreachable code.

**Fixed.** Four verbs, implementing verbs the design had already admitted (so no
§6 pass was needed, and no object was added):

| Verb | Does |
|---|---|
| `SYS_CAP_DERIVE` (50) | Narrow rights and/or budget into a child capability in the caller's own table. Widening is refused - the subset test runs against the parent's *stored* rights, so there is nothing a caller can pass to defeat it |
| `SYS_CAP_REVOKE` (51) | Bump the object's epoch: every outstanding capability to it dies, in every cell, O(1) |
| `SYS_CAP_INFO` (52) | Report object / kind / rights / budget - so a cell can **check** an attenuation instead of assuming it |
| `SYS_CAP_DROP` (53) | Release one capability from this table |

Three design points worth keeping:

1. **`REVOKE` is its own right** (`rheo-abi::RIGHT_REVOKE`), minted to an
   object's creator and withheld from a derivation unless asked for and already
   held. Handing a peer read access to a buffer must not also hand it the power
   to invalidate that buffer for everyone. This is the bit the proof's most
   discriminating phase turns on.
2. **The verbs take the 32-bit ABI `cap_id`**, not the kernel's 64-bit `Handle`
   - the form every other part of the ABI already uses, and the only form a cell
   ever receives. Taking the 64-bit form would have been a quiet trap: the two
   are numerically equal while a slot's generation stays below 2^16, so it would
   have worked in every test and started failing after 65536 reuses of one slot.
3. **Neither `SYS_CAP_INFO` nor a derivation spends budget.** `grant_check`
   decrements, so introspecting a metered capability through it would consume
   the thing being looked at. A derivation may narrow a budget but never exceed
   the parent's, or metering would be escapable by deriving.

The rights bits moved into `rheo-abi` at the same time - they were written out
by hand in both `kernel::capability` and `runtime::rights`, which was tolerable
while only the kernel named them and is not now that a cell chooses them (§3.1).

**Proof**: `security`'s F4 phase, on all three ISAs. A real unprivileged U-mode
cell runs eight checks and reports a bitmask; the kernel then asserts what the
cell cannot fake - the object's **epoch actually moved** in the kernel's own
table (which the cell has no mapping for and no verb that reports it), the
kernel's own copy of the revoked capability is dead, and a capability to a
*different* object is untouched. Both new enforcement points were observed
failing when reverted independently: dropping the `REVOKE` requirement gives
`0xCF` (bit 4, "the child could revoke"), dropping the widening check gives
`0xF7` (bit 3).

**Still open in this section**: `delegate` - moving a capability to *another
cell's* table - has no verb yet. It is the one that needs §2.3 first, because
with a shared capability table there is no other table to delegate into.

This section is also the **hard prerequisite for the identity model**
(docs/IDENTITY.md 9): "root holds a maximal capability bundle" and "dropping
privileges revokes" are claims, not mechanisms, without these verbs. That half
is now real; the per-child-table half (§2.3) is not.

### 2.2 Revocation is vacuous for memory grants (A)

`revoke_epoch` bumps `Object::epoch`, which makes future *grant checks* fail. But
after `SYS_COMMIT` a cell reaches its pages **through the MMU, not a grant
check**, and `grant_share` (`user.rs:648`) maps the same frames into the peer.
Nothing unmaps on revoke. §8.2 property 3 therefore holds **vacuously**: no
capability check stands between the peer and the bytes.

The machinery to fix it exists (`AddressSpace::unmap`,
`paging_for_each_user_leaf`); property 3 also needs restating to mean what the
design needs.

**Unchanged by §2.1.** `SYS_CAP_REVOKE` makes epoch revocation *reachable*, and
the `security` F4 phase proves it kills every **capability** to the object -
including one derived from it. It does not unmap a single page. So for a grant
whose frames a cell already reached through `SYS_COMMIT`, revoke still removes
the right to ask and leaves the bytes: the gap this section describes is exactly
as wide as it was, and is now easier to hit, because a cell can trigger it.

### 2.3 A spawned cell shares its parent's capability table - **CLOSED**

`install_spawned` copied the parent's pointers verbatim. Consequences:
`abi.rs`'s claim that spawn authority is "not minted into a spawned child by
default" was **false** - every descendant inherited it; and §8.2 property 4
(disjoint capability sets) was **inapplicable** to any parent/child pair, or to
the whole service fan-out where four cells shared one table. What actually
isolated memory was the per-cell grant array plus the page tables - so in the
shipped code the capability table was not the isolation boundary the design says
it is.

**Fixed.** A kernel-owned `CELL_CAPS[MAX_CELLS]` (fixed static, so the kernel
stays allocation-free) backs every cell the *kernel* creates:

- **`SYS_SPAWN`** gives the child an **empty** table. Whatever it legitimately
  needs - its queue pair, an inherited channel - is minted into that table
  explicitly by the spawn path. It is a list, not an inheritance, and the
  parent's `ObjectKind::Cell` capability is simply not on it.
- **`fork`** gives the child a **copy** of the parent's table, for the same
  reason POSIX copies the descriptor table: the child holds what the parent held
  *at the fork*, and neither can change the other's holdings afterwards. Epoch
  revocation still reaches both, because that lives on the object.
- The **object** table stays shared, on purpose: it is one per system
  (`ARCHITECTURE.md` §3), the registry objects live in, not an authority.
  Reaching an object still needs a capability in the calling cell's own table.
- `free_cell` empties a reaped slot's table, so a reused slot cannot inherit a
  dead cell's authority by accident.

The mint had to move *after* `install_spawned` - the child's queue capability
now has to land in a table that does not exist until the child is installed.

**Proof**: `librheoproc`, on all three ISAs. Every spawned `/bin/child` tries to
spawn `/bin/echo` - a path that exists and that its **parent** spawns
successfully in the same scenario, so the refusal is about authority and not
about the file. The oracle is self-normalising (refusals must equal child runs,
so it does not have to know that the orchestrator also runs an 8-iteration spawn
benchmark) and it fails if even one child gets through. Observed failing when
reverted: restoring the shared pointer prints `SPAWNED WITHOUT AUTHORITY`
eleven times. All five spawn/fork-heavy kernels - `librheoproc`, `librheopipe`,
`netservice`, `librheowl`, `linuxproc` - pass **unedited**.

This unblocks `SYS_CAP_DELEGATE` (§2.1's one remaining verb), §2.2, §2.6, and
`docs/IDENTITY.md` ID2.

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
not move, they register themselves. The next step - the tables' implementations
moving into driver cells reached over channels, "message-driven service later"
made real - is owned by DRIVERS.md 4.4.

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
Bun-compiled native ELF (JavaScriptCore), dynamically-linked glibc; npm only
bootstraps it. Two immediate consequences: **Bun has no riscv64 build**, so the
three-ISA invariant cannot hold for this goal (x86-64 + aarch64, riscv64
skip-with-reason); and `io_uring` is no longer a blocker (Bun >= 1.0.16 uses
epoll).

### 4.0 The binary, measured

Everything below used to be estimated from the package description. It is now
**measured against the real artifact** - `@anthropic-ai/claude-code-linux-x64`
**2.1.220**, fetched from the npm registry, `readelf`'d, and `strace`'d running
`claude --version` to completion on this host. The estimates were wrong in ways
that change the plan, which is the reason to measure before building.

| Property | Measured | Previously assumed |
|---|---|---|
| Size | **275,012,592 B (262 MiB)** | "~100 MB" - **2.6x under** |
| ELF type | **`ET_EXEC`, non-PIE**, with `PT_INTERP` | (unstated) - the L2 stock-base path, not the L7 PIE path |
| Base VA | `0x200000` (2 MiB) | (unstated) |
| Interpreter | `/lib64/ld-linux-x86-64.so.2` (glibc) | glibc - correct |
| `PT_LOAD` memsz | 25.2 MiB `R` + 55.2 MiB `RX` + **182 MiB `RW`** = **262 MiB** | (unstated) |
| `PT_GNU_STACK` memsz | **`0xc35000` = 12.8 MiB** | (unstated) |
| `PT_TLS` | filesz `0x23b8`, memsz `0x69f1` (has `.tbss`) | (unstated) |
| Distinct startup syscalls | **41** | (unstated) |
| SIMD | AVX2 **and AVX-512** (`vmovdqu64` x857) | "AVX2 baseline" - **AVX-512 not expected** |

Four blockers followed, and they were facts rather than predictions. **Three are
now closed** (1, 3 and 4), and the fourth - **blocker 2, eager paging** - is
**partly** closed: file-backed `mmap` is demand-paged, the ELF image is not yet.
Its old framing ("this binary is too big") was wrong and is corrected in place;
so was its claim of 182 MiB of `.bss`, which measurement shows does not exist.

1. ~~**The stack is too small by 4.8 MiB.**~~ **CLOSED.** `PT_GNU_STACK` asks
   for 12.8 MiB and `stack::LINUX_STACK_PAGES` was 2048 = 8 MiB. The fix is not
   a bigger constant - raising it to 16 MiB would have fitted this binary and
   left the next one to fail identically. `elf::stack_size()` reads
   `PT_GNU_STACK`'s `p_memsz`, `load::LinuxImage` carries it, and
   `stack::stack_pages_for()` maps `max(request, 8 MiB)` up to a **64 MiB
   ceiling** - a bound, because the stack is mapped eagerly and charged to the
   cell's frame budget, so a 2 GiB request would exhaust the pool at load with
   no diagnostic; above the ceiling it is clamped **and logged**.

   `LinuxState.stack_pages` carries the result and `rlimit_for` reads it, so
   `RLIMIT_STACK` reports what was actually mapped. That second half is not
   cosmetic: glibc sizes *thread* stacks from that number, so reporting more
   than is mapped hands every thread a stack that faults.

   `PT_GNU_STACK`'s flags also carry executable-stack permission, deliberately
   **not** read - W^X is structural here, and an executable stack is refused
   rather than honoured.

   Proven by the `stackx` fixture in `linuxproc` on all three ISAs (linked
   `-z stack-size=12582912`, writes through 9280 KiB in 145 recursive frames).
   Each half was observed failing when reverted alone: with the mapping reverted
   the run prints **nothing at all**, because the fault eats glibc's buffered
   stdout - which is exactly why the original defect was hard to attribute.
2. **Eager paging.** *Closed for the two paths a large image travels: file-backed
   `mmap` and the ELF image itself. `fork`, the initial stack and `execve` remain
   eager and are named below.*

   The framing this section used to carry was wrong, and worth correcting rather
   than quietly rewriting: 262 MiB is **unremarkable for modern software**, and the
   defect was never the binary's size. It was that the kernel *eagerly
   materialized what Linux demand-pages* - a mapping cost what it reserved rather
   than what the program touched - which is the wrong design at any size. Framing
   it as "this binary is too big" points at a bigger pool; framing it correctly
   points at demand paging.

   Also corrected by measurement: this section claimed "182 MiB of `.bss`".
   `readelf` says `filesz == memsz` for **all three** `PT_LOAD`s, so there is **no
   `.bss`** - the whole image is file-backed. Anonymous demand paging, which was
   the planned first slice, would have covered none of it.

   **Done:** file-backed `mmap` is demand-paged. `arch::paging_mapped` +
   `AddressSpace::is_mapped` let the fault handler tell an absent page from a
   refused access (`FaultCause` has no read/write bit, so the page tables are the
   source of truth - and a permission fault mistaken for a missing page
   repopulates and re-faults forever, measured at 78,780 fills in the revert
   probe). `linux::filemap` holds the VFS handles that *mappings* own, because
   `ld.so` closes the fd immediately after `mmap`. `Vma` carries the backing and
   offset under a one-reference-per-record rule. Proven by `mmapdp` in `linuxproc`
   on all three ISAs (64 pages mapped, 5 filled) and by `linuxdyn`, where `ld.so`
   maps a real 1.5-2.1 MB `libc` and an unmodified dynamic glibc binary runs.

   One of that rule's two halves was **missing at HEAD, independently of the image
   work**, and is now fixed and shipped: `mmap` of a file survives `fork`, and
   `dup_state` copies a whole `LinuxState` with one `copy_nonoverlapping` that
   touches no refcount. It addref'd pipes and nothing else, so a child's records
   named backing-store entries it held no reference to; the child's exit gave back
   one per record, the entry was freed, and the **parent** - which did nothing wrong
   - then faulted on an untouched page and got zeros. `VmaList::inherit_files()`
   takes the reference at `fork` and `process_exit` clears the exiting process's VMA
   list where it already reclaims its frames, so the lifetime is symmetric. Both
   halves are proven by `mmapdp`: after the forked sharer exits, the parent reads a
   page no phase has touched and asserts its own file byte. Observed failing when
   reverted - `dp: page 20 reads 00 ... want 54` without the inherit, and a registry
   still holding an entry at the end of the run without the release.

   That slice also uncovered and fixed a **latent x86-64 defect**: a ring-3 fault
   resumed through `sysretq`, which consumes RCX and R11. Harmless while signal
   delivery was the only fault resume (a handler entry does not re-execute
   anything); fatal for the first path that does. Faults now resume through
   `iret_resume`, restoring every register.

   **Blocked, with the blocker identified:** extending demand paging to the ELF
   image was written and reverted, because it exposed a **prerequisite**. With the
   image lazy, a cell passes a pointer into its own untouched rodata to `write` and
   the *kernel* dereferences an absent user page - a load fault at a kernel PC, which
   is not resumable here. That is why Linux has `copy_from_user` with a fixup table.
   **That prerequisite is now closed.** The F1 hardening already routes every
   cell-supplied pointer through one set of helpers, so those gained "ensure present"
   beside "in range" - the same question a `copy_from_user` fixup answers, asked once
   at the seam instead of at every dereference. The placement was the whole problem:
   putting it in the bare `user_read_ok`/`user_write_ok` predicates cost a **~2,900x**
   amplification (11,516 of 11,520 demand fills came from the kernel), because
   `unmap_range` uses them purely to *bound* a range and so materialised every page
   immediately before freeing it. Moving it to the helpers that hand back something to
   **dereference** brings it to **0** kernel pre-faults, measured every run.

   **Done: the ELF image.** `load::load_elf_linux` now **records** the `PT_LOAD`s the
   fault handler can fill (a `load::SegRecorder`) and copies only the ones it cannot,
   printing which segment and why each time it declines. Two conditions, each found by
   a segment that broke without it: `p_filesz == p_memsz` (a `.bss` tail inside one
   record produced a null dereference in a static Rust binary), and `p_offset`
   congruent to `p_vaddr` mod the page size (paging fills whole pages). Because the
   bytes are already resident in kernel memory rather than in a file, `filemap` gained
   a second store kind; because a segment's content ends mid-page, `Vma::file_len` says
   how far a record is backed, so the tail of its last page is zeros rather than the
   next segment's bytes. Measured on riscv64: `rusthello`'s 201 image pages cost **16**
   frames at load instead of 201, and `linuxrun` asserts that inequality on all three
   ISAs. `linuxdyn` passes, so `ld.so` relocates a demand-paged program.

   Four failures were found on that path and **all four are now closed**:

   1. **Reset ordering.** `user::reset()` after loading cleared the registry, so every
      page came back zero - an illegal instruction at the entry point and nothing more
      informative. Reset now runs before the load in the harness and the three test
      kernels that load directly, and `filemap::alive` makes a recurrence print
      "reset before loading, not after" instead of handing out blank memory.
   2. **A `.bss` tail** in one record - scoped out by the `filesz == memsz` rule above.
   3. **The fork refcount**, which turned out to be a live defect independent of this
      work and is **shipped separately with its own proof** rather than parked behind
      it (see the paragraph above).
   4. **The 4 -> 61 fill count**, which was the **oracle**, not the handler. The
      assertion read `mem::faults()` - the total across the whole address space - and
      once the image is lazy that total legitimately includes the program's own text
      and data (61 on riscv64, 68 on x86-64, 67 on aarch64: the pages the eager path
      used to allocate up front). `faults_mmap()` exists precisely for this and its
      doc comment says so. The assertion now reads the region count - 5, hand-computed
      from the fixture's touches - and prints the total beside it. The lesson is
      recorded in ENGINEERING.md 11: a metric that was a valid oracle can stop being
      one when the system around it changes, and the failure looks like a regression.

   **Done: `execve` and the ELF interpreter.** Both stream from the VFS, and the
   obstacle was never the streaming - it was recording against the *caller's* fd, which
   is closed on return. A mapping now opens its **own** handle over the path (the `mmap`
   precedent), `SegRecorder` holds two stores because a dynamically linked program is
   two files, and `exec_reinit` records as `install_cell` does. `linuxdyn` records 1
   program + 1 `ld.so` segment (35 pages recorded, 4 copied); `linuxproc`'s fork+execve
   phase records 221 and copies 21. Both run inside a syscall where a test can measure
   nothing directly, so `load::recorded_pages()`/`eager_pages()` are the witnesses, and
   both assertions were observed failing when reverted.

   That slice also **introduced and fixed a regression the matrix caught**:
   `exec_elf_from_vfs` is shared with the *native* `SYS_SPAWN`, which has no VMA list,
   so its child got a lazy image nothing mapped and `librheoproc` failed on x86-64 with
   `echo_ok=0`. The eager and lazy loads are now separate functions - the eager one
   keeps the old name, so an unaware caller gets a correct image, and the lazy one is
   named for the obligation it imposes (ENGINEERING.md 11).

   **Done: `fork` is copy-on-write.** It shared 2406 pages, copied 0, and cost 12
   frames of child page tables for a 9.4 MiB process on riscv64 - 200x. A per-frame
   refcount in `frames` (`free` becomes a decrement, so every pre-COW caller is
   unchanged), a software PTE bit per ISA (`paging_cow_protect_user`/`_at`/`_clear`),
   and a fault branch that privates a page on write. The mark lives in the page table,
   not the VMA list, so it covers the stack and `brk` heap that have no VMA record; the
   parent is write-protected too, the half that fails silently. Proven by `cowfork`,
   both halves observed failing when reverted.

   Doing it exposed and fixed an **architectural gap**: the kernel touched cell memory
   at ~98 sites and 51 dereferenced the raw VA with only a bounds check done elsewhere,
   so each lazy-mapping feature re-opened a 98-site audit at a new strength. All 51 now
   route through `kernel/src/uaccess.rs`, the single seam that enforces bounds,
   presence and COW resolution - a new lazy feature changes one function there. The
   split between kernel *mechanism* (refcount, share, cow-protect, fault delivery) and
   COW *policy* (personality code) is kept visible so the policy can move behind a
   userspace process server later, the seL4 way.

   **Done: the stack grows on fault.** `setup_stack` maps only the top page (argv/envp/
   auxv) and registers the rest of the `PT_GNU_STACK` request as an anonymous RW
   reservation; a touch below the top page faults in, a touch below the reservation is a
   SIGSEGV (the guard page from the bound, not a dedicated page). `stackx` proves it -
   a 12 MiB request's 9280 KiB of writes appear as 2380 demand fills, 59 when the eager
   mapping is restored. That closes the last eager path: image, file `mmap`, `fork` and
   stack are all lazy.

   **Still eager, and named:** a segment with a `.bss` tail, and every **native** cell's
   image. Both ride this same handler.

   **What remains before the real target can be attempted** splits into two
   independent sub-problems, and measuring the tree sharpened which one is actually
   open (task #167):

   (a) The binary cannot live in the **kernel image** - a 275 MB `include_bytes!` runs
   past the frame-pool base 64 MiB into RAM. *Effectively solved and proven:*
   `linuxproc` already `execve`s the unmodified 4-4.7 MB coreutils multicall from a
   mounted VFS, demand-paged, and `blockfs` proves virtio-blk -> ext4 -> VFS. A test
   that merely slurped a small ext4 off the disk and loaded from it would combine two
   proven things and add nothing.

   (b) The binary's bytes cannot all reside in **RAM at once** - it was the open one,
   easy to miss, and it is **now closed** (#167). It used to be true that `posix::Ext4`
   was pure over an in-RAM `&[u8]` (and `blockfs` slurped the whole disk with
   `Vec::leak`), so mounting a 275 MB image meant 275 MB resident *regardless of demand
   paging* - demand paging makes the **cell's** pages lazy, not the **source**. The fix
   is a **block-cached ext4**: `posix::BlockSource` (byte-addressed `read_at` into a
   caller buffer) is the seam; `kernel::hw::block::BlockCache<D>` is a fixed,
   allocation-free LRU of `LINE`-byte lines (`CAPACITY = LINE*LINES`) over a
   `BlockDevice`; and `Ext4` reads every field/extent/directory through the source
   rather than borrowing a whole-image slice. `blockfs` now mounts the live disk through
   the cache and asserts the streaming property directly: the 7800-byte multi-block file
   reads correctly through an **8 KiB** cache over a **512 KiB** disk (`CAPACITY <
   disk`), with `block::cache_fills() > 0` proving the bytes came from the device on
   demand, not a preload. An in-RAM `&[u8]` is still one `BlockSource` (the `posix`
   kernel's path, unchanged), so both the resident and the streaming source are proven.

   The full **`execve`-off-a-live-disk** composition is **now proven** (GOAL-DISK-2b),
   and the ext4 driver is now the `ext4plus` crate (`ext4fs`; the hand-rolled parser was
   retired - see the ext4plus paragraph below). Two rungs came together: (1) the
   streaming `execve` path (`load::exec_elf_inner`) parses `PT_INTERP` and streams the
   interpreter demand-paged, the same handling the from-slice `load_elf_linux` had,
   factored to share `stream_elf_at`; (2) the ext4 driver reads through the
   `BlockSource`/`BlockCache`. `linuxdyn` proves it in three phases on all three ISAs:
   phase 1 loads `dhello` from a slice, phase 2 `execve`s it from a ramfs VFS, and
   **phase 3 `execve`s it from a real ext4 image on a live virtio-blk disk** - the
   program, its `ld.so` interpreter and `libc.so.6` all stream off the disk on demand
   through the 8 KiB block cache (447-590 cache fills, exact stdout + exit 12), none
   resident whole. That is a dynamically-linked glibc binary running unmodified,
   `execve`d straight off ext4 - the shape a shell launching Claude Code needs.
   `MAX_MAPPED_FILES` was raised 8 -> 64 (a documented limit-raise), headroom for a
   production binary's dozen-plus shared libraries. Cost,
   measured not assumed: a `read_at` for a 2-4 byte field is one LRU lookup, a miss is
   one `LINE/SECTOR`-sector device read, and a data read copies straight from the
   covering line. Composing the streamed mount with the demand-paged loader is the next
   rung.
3. ~~**Seven syscalls the personality does not dispatch**~~ **CLOSED.** Measured
   from the real startup trace rather than guessed, and all seven now dispatched:

   | Syscall | Calls | What landed |
   |---|---|---|
   | `open` (x86-64 legacy **2**) | 2 | **Was a genuine defect of the two-numbers class** (`ENGINEERING.md` §11): glibc issues legacy `open` in preference to `openat`, and the personality implemented only `openat`, so every `open` was refused on x86-64 **and nowhere else** - the same trap that made `readlink` fail there alone. Now routed to `openat` with `AT_FDCWD`; an unreachable sentinel on the asm-generic tables, covered by the existing uniqueness guard |
   | `eventfd2` | 1 | **The load-bearing one**: the epoll event loop's only wakeup path, so refusing it does not degrade the program, it removes the mechanism. `kernel/src/linux/eventfd.rs` - a 64-bit counter as a per-cell fd indexing a per-personality registry (the `linux::epoll` / `linux::pipe` pattern, **no kernel object**). The counter is in the registry, **not** in the `FdKind` variant: `dup`/`fork` make a second descriptor for the *same* object, and a counter copied per descriptor gives two counters that silently stop waking each other. `EFD_SEMAPHORE`, poll/epoll readiness, and a blocking read that parks through the same runnable-peer rule a pipe read uses |
   | `sysinfo` | 2 | Real numbers, from the frame pool and the cell's own clock domain. Bun sizes its heap from `totalram`/`freeram`, so a zeroed answer is worse than a refusal. `sharedram`/`bufferram`/highmem/swap/`loads` are genuinely 0 here - the true values, not placeholders. `struct sysinfo` is identical on all three LP64 targets, so it lives in portable code with its 112-byte ABI size asserted |
   | `sched_setscheduler` | 4 | Honest rather than accepted-and-dropped. One scheduling class exists here (cooperative round-robin), so `SCHED_OTHER` at priority 0 succeeds *because it is already in force*, and `SCHED_FIFO`/`RR` are refused `-EPERM` - the errno an unprivileged Linux process gets, which every caller handles. Telling a program it got real-time scheduling on this scheduler would be a lie. `sched_getscheduler` and `sched_get_priority_{max,min}` came with it |
   | `close_range` | 1 | glibc falls back to a `close` loop, so this is a *performance* call - but it must do the thing, and it does. `CLOSE_RANGE_CLOEXEC` honoured; `CLOSE_RANGE_UNSHARE` refused rather than ignored |
   | `clone3` | 5 | Constant existed but was never dispatched, so it logged `ENOSYS nr=435` as if the number were unknown. It is known, and refusing it is the *correct* answer - glibc falls back to `clone` - so it now says so deliberately |
   | `rseq` | 6 | Same shape: known, and "no restartable sequences" is glibc's documented fallback |

   Proven by the `sysx` fixture in `linuxproc` on all three ISAs, which asserts
   each refusal **as** a refusal. Four narrow reverts were each observed failing:
   removing the `nr::OPEN` arm (x86-64 only), making an eventfd always report
   pollable-readable, ending `sched_setscheduler` in `_ => 0`, and zeroing the
   `sysinfo` totals. The legacy-`open` transcript line differs by ISA on purpose -
   syscall 2 exists only on the x86-64 table, and that ISA-only existence *was*
   the defect.

4. ~~**AVX-512 is in the binary.**~~ **NOT A BLOCKER - measured and
   eliminated.** The 857 EVEX instructions sit behind runtime CPU dispatch, and
   the binary's actual floor is far lower than assumed.

   The test: `qemu-user` (`qemu-x86_64`, installed for this) running
   `claude --version` under a series of CPU models. Same binary, no
   modification, exit status and output both checked.

   | `-cpu` | ISA level | Result |
   |---|---|---|
   | `max` | everything TCG has (AVX2, no AVX-512) | `2.1.220 (Claude Code)` |
   | `Skylake-Client` | AVX2, **no** AVX-512 | `2.1.220 (Claude Code)` |
   | `Haswell` | AVX2, **no** AVX-512 | `2.1.220 (Claude Code)` |
   | `IvyBridge` / `SandyBridge` | AVX, no AVX2 | `2.1.220 (Claude Code)` |
   | `Westmere` / `Nehalem` | **SSE4.2**, no AVX | `2.1.220 (Claude Code)` |
   | `qemu64` | SSE2 baseline | **SIGILL** |

   So the **real floor is SSE4.2 (Nehalem)**, not the "AVX2 baseline" this
   section used to assert - and not AVX-512 either. Corroborating static
   evidence: the ELF carries **no** `GNU_PROPERTY_X86_ISA_1_NEEDED` note (the
   linker recorded no required ISA level above baseline), and `.text` contains
   **249 `cpuid`** and **23 `xgetbv`** sites, which is what runtime dispatch
   looks like.

   `xtask` already passes `-cpu max` for x86-64, which is several levels above
   the floor. Nothing to do.

   Worth keeping as method: this was the *cheapest* of the four blockers to
   settle and it gated whether the other three were worth doing under emulation
   at all, so it went first. Answering it needed one `apt-get install qemu-user`
   and six runs - against an unbounded amount of speculation.

**What is *not* a blocker, contrary to the earlier note here**: the 120 s test
timeout is an `xtask` constant, not a property of the world, and a `--version`
run costs well under a second of syscall time on the host. The KVM lane (§4.1)
matters for *interactive* rungs, not for proving that the binary loads and
starts.

Three blockers are **design decisions**, not unfinished work:

1. **W^X is structural** - *the silent part is now closed; the doctrine question
   is still open.* `MapPerm` has three variants and no RWX (`arch/mod.rs:44-51`),
   by design (ARCHITECTURE.md 5). But `mmap(PROT_READ|WRITE|EXEC)` **returned
   success while silently dropping EXEC**, so JSC - which maps its JIT pool RWX on
   Linux - would fault on its first jump into generated code with **no diagnostic
   near the cause**.

   Fixed: `mmap` and `mprotect` refuse `PROT_WRITE | PROT_EXEC` with `-EPERM` and
   a printed reason. That is the answer a caller can act on, because the `mprotect`
   RW->RX **flip** path works - a flipping JIT is viable where an RWX one is not,
   and silently dropping the bit removed that choice. `mmapx.c` asserts the
   refusal on both syscalls *and* that the flip path works, on all three ISAs;
   observed failing reverted (*"PROT_WRITE|PROT_EXEC was accepted"*).

   **Still a decision, not a task:** whether to add a `UserRwx` variant. It needs
   the ARCHITECTURE.md 6 admission pass - W^X is a constitutional property here,
   not an implementation detail - and the alternative (run Claude Code's JIT off
   via its environment) trades a doctrine change for a large performance loss on a
   JavaScript workload. Deliberately not decided in a patch.
2. **No VMA list at all** - *the silent-corruption half is now closed.* `mmap` is
   a forward bump cursor (`mem.rs`), so nothing detects placement collisions. The
   cursor was also **unbounded**, which is what made it dangerous rather than
   merely crude: a long enough run of allocations left the 12 GiB mmap region,
   crossed the cell's queue-pair region at 16 GiB and its channel regions at
   24 GiB, and reached `LINUX_INTERP_BASE` at 64 GiB where `ld.so` and `libc.so.6`
   live - handing a program addresses aliasing its own dynamic linker with no
   error. 4 GiB of mappings is enough to get there.

   Fixed: the region is bounded at the queue VA (`MMAP_END`), `mmap` and `mremap`
   both report `-ENOMEM` past it, and a caller-chosen `MAP_FIXED` overlapping the
   queue or channel regions is refused `-EINVAL` with a printed reason - that is
   the one case a bump cursor cannot protect against. The interpreter's own span is
   deliberately *not* in the refusal set, because `ld.so` legitimately maps within
   it (L7). Proven by `mmapx.c` in `linuxproc` on all three ISAs, observed failing
   reverted (*"oversized reservation was accepted"*).

   **The VMA list is now done too.** `kernel/src/linux/vma.rs` keeps one record
   per mapping (base / len / prot / flags) in a fixed 128-entry per-cell array -
   per-cell synthesized state, **no kernel object**, like the fd table and the
   process tree. Placement is **first fit over that list**, so a freed span is
   reusable; `munmap` and `mprotect` split records at the edges, so a partial
   unmap in the middle of a mapping produces the two pieces that survive instead
   of one record claiming to own a hole; adjacent records with identical
   protection merge, which keeps a program that maps many small ranges from
   exhausting the table; `fork` copies the list (the child's address space is a
   copy, so its map of it must be too) and `execve` clears it.

   `mmapx.c` asserts the two properties a bump cursor **cannot** have, and both
   assertions are on an *address*, which is the only kind that can tell the two
   designs apart: a freed middle span is handed back at **the same address** and
   is writable, and a partial unmap leaves both ends intact with the hole
   reusable. Observed failing when reverted to a search that starts above every
   live mapping - `got 0x300041000, freed 0x300021000`.

   That revert is worth recording as a **hazard, not just a control**: the first
   version of this kept the old cursor as a "hint" for first fit to start from,
   on the reasoning that an allocation-heavy program should not rescan the low
   end of the region. That silently restored the exact behaviour being removed,
   because a search starting past every mapping can never find a hole behind
   one. The feature was present, well-commented, and did nothing. What caught it
   was the fixture asking for a specific address rather than for success.

   **Still open for Claude Code**: demand paging (blocker 3), which is what this
   list was the prerequisite for - the fault handler's "which region is this, and
   with what protection?" is a `VmaList::find` call. The user VA ceiling is
   **256 GiB on every ISA** (`user.rs`, Sv39's user half applied uniformly).
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
- ~~**Cross-process `kill`: written, then reverted unshipped.**~~ **CLOSED**, and
  it now passes on **all three ISAs including x86-64**, the one it failed on
  before.

  `kill` refused any pid but the caller's own with `-ESRCH`, and answered
  `kill(0, sig)` / `kill(-1, sig)` by **silently delivering to the caller**. Now:
  a pid is looked up in the process table (`cell_of_pid`); the signal is resolved
  against the **target's** disposition table and recorded pending on its main
  context; and it is delivered from `reschedule` immediately after switching in
  (`signal::on_resume`) - a frame rewrite pushes a `rt_sigframe` onto the
  *target's* stack, so the target's address space must be active, and that is the
  only moment it is. `kill(pid, 0)` is a real existence probe; `kill(0)` fans out
  to every live process (there is no `setpgid`, so every process genuinely *is*
  in the initial group); `kill(-1)` fans out **excluding the top of the tree**,
  standing in for init; a negative pid other than -1 names a group that does not
  exist and is refused rather than redirected to the caller. An uncaught fatal
  default on a non-running target becomes a zombie its parent reaps, without
  recursing back into the scheduler.

  Two deliberate details. `on_resume` runs **after** `complete_block`, so the
  interrupted syscall's return value is already in the frame and gets saved into
  the ucontext - the reason it does not reuse `check_pending_current`, which
  clobbers the return register with 0 and would turn a completed read into a
  spurious end-of-file. And the pre-fix behaviour was reverted **twice**,
  separately, to check each discriminating phase alone: the child probe
  (*"probe of a live child failed"*) and the init exclusion (*"-1 with no targets
  did not report ESRCH"*).

  **Why it failed on x86-64 last time is still not known**, and that is worth
  stating rather than papering over: this is a rewrite against the same design,
  not a located-and-fixed bug. The most likely explanation remains the one the
  `readlinkat` half of that slice was root-caused to - **glibc on x86-64 issues
  the legacy `readlink` (89), not `readlinkat` (267)** - which would have made the
  *fixture* fail there for a reason unrelated to `kill`, since the two shared one
  binary. `kill` is 62 on x86-64 and 129 on asm-generic and both were already
  dispatched, so it was never that number.

  The general hazard, worth its own line: **a syscall that exists under two
  numbers is a portability trap that only one ISA exercises.** x86-64's legacy
  arms (`access`, `pipe`, `dup2`, `readlink`, ...) are issued by glibc *in
  preference to* the `*at` forms, so an implementation written against the
  asm-generic table passes on aarch64 and riscv64 and silently does nothing on the
  ISA that matters most for this goal.

  Two notes on method, both earned here and both kept in the fixture:
  - The first three `kill` phases written (self probe, absent pid, unknown group)
    all passed **with the fix reverted** - the old stub happened to give the same
    three answers. A proof that does not discriminate is not a proof, and the
    only way to find that out is to revert and re-run. That is what produced the
    child-signal and `kill(-1)`-spares-init phases, which do discriminate.
  - Two mechanisms could let a child wait while the parent signals it, and both
    turned out to be broken - the next two entries. Neither was known before
    trying to write this proof; one of them (`sched_yield`) is now fixed, and it
    is what the child in this fixture waits on.

  **Scope, honest:** a signal to a target **blocked** in a syscall is recorded
  pending and delivered when that syscall's own condition is satisfied - it does
  not interrupt the wait with `EINTR`. A signal cannot currently cut a process
  out of a blocking `read`. That needs `complete_block` to have an interrupted
  path and is a separate slice.
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
  `sched_yield` loop. The third is explained by the next entry - and that entry is
  now **closed**, so a bounded-`sched_yield` child is a viable design again and is
  the cheapest way to re-open this. The first two are still unexplained.

  Recorded this way on purpose. The inference was published one step ahead of the
  evidence, which is the same mistake `ENGINEERING.md` §1 exists to catch when
  someone else makes it - a plausible mechanism is not a diagnosis. Whoever picks
  this up should start from the observation, not from the retracted cause.
- ~~**`sched_yield` does not yield across cells (B).**~~ **CLOSED.** It rescheduled
  among a cell's own contexts (L4 threads) only, so a child looping
  `sched_yield()` ran to completion before the parent was scheduled at all - in a
  cooperative scheduler a yield is one of the few preemption points, and this one
  did nothing. `sched_yield` now falls through to `proc::yield_cell`, the same
  cross-cell hand-off `wait4`/pipe-block/exit already use, with the caller left
  **runnable** rather than blocked - what the native `SYS_YIELD` does
  (docs/NETSTACK.md 17). The round-robin visits the caller last, so a yield by the
  only runnable process is never a block and never a deadlock.

  It was **two** defects stacked, and the first hid the second. `pick_next` scans
  a full lap, so its last candidate is the caller itself - the running context is
  `Ready` ("currently running, **or** waiting for its turn"). Every other caller
  reaches it with the caller already `Blocked` or `Free`, so only a yield could
  pick itself, and picking itself made the call a no-op: `switch_to(cell, ci, ci)`
  saves and reloads one FP image and returns the same frame. A single-threaded
  process therefore never reached the "no sibling ready" arm at all, which is why
  adding the cross-cell fallback alone changed nothing. Both halves were observed
  failing independently.

  `yieldx.c` is the proof, and the witness is an ordering record neither side can
  fake: parent and child run the **identical** loop - write one marker byte to a
  shared pipe, yield, eight times - and a pipe is one cross-cell ring (L6), so the
  byte order in the ring *is* the interleaving. `fork` returns into the parent
  first, so the hand-computed oracle is `PCPCPCPCPCPCPCPC`; pre-fix the parent's
  yields did nothing and it wrote all eight `P`s before blocking in `wait4`
  (`PPPPPPPPCCCCCCCC`, observed). The two differ at the first transition.

  This closes the third of the three child designs in the entry above; the two
  pipe-based ones are still unexplained. (`poll`/`epoll_wait`/`nanosleep`,
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

### 4.1 The tooling constraint, measured

Rungs past a "hello" will not fit the 120 s test timeout under TCG, and nothing
in the tree uses KVM. That made "a KVM-accelerated lane" a named prerequisite.

**Measured, and it is not available here.** The development container is itself a
hypervisor guest with **no nested virtualisation exposed**: `/proc/cpuinfo`
reports the `hypervisor` flag and **zero** `vmx`/`svm` flags, and `/dev/kvm` does
not exist. QEMU 8.2 lists `kvm` among its accelerators, which is a property of
the binary, not of the machine - `-accel kvm` cannot open the device. So a KVM
lane can be *written* here but never *run* here, and writing a lane whose only
proof is on someone else's hardware is a claim, not a capability.

The consequence for the plan is specific, and it is better than it looks:

- The ladder's **large-binary** rungs (unpatched Bun, ~100 MB, AVX2) are blocked
  on hardware this container does not have. That is an honest external
  dependency, recorded here rather than worked around.
- The ladder's **mechanism** rungs are not blocked at all. Demand paging does
  not need a 100 MB binary to prove - it needs a fault, a fill and a resume, and
  a fixture that touches one unmapped page proves it exactly as well as a fixture
  that touches 25,000. Same for a real VMA list, first-fit placement, span reuse,
  and `MAP_NORESERVE`.

So the order changes: **build the mechanisms and prove them small**, and let the
large-binary rungs wait for hardware. That is also the right order on merit -
eagerly copying a ~100 MB image page-by-page into private frames is the thing
demand paging exists to delete, so proving demand paging first means the
large-binary rung is a *measurement* when it finally runs, not a debugging
session.

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

**Now - correctness.** ~~§2.4 the scheduler idle state~~, ~~§2.5 a system-wide
admission ledger~~, ~~§2.1's derive / revoke / inspect / drop surface~~ and
~~§2.3 per-child capability tables~~ are **closed** (§1). Next: **§2.2 complete
revocation** for memory grants (revoke invalidates capabilities today and unmaps
nothing, so §8.2 property 3 still holds vacuously), then **`SYS_CAP_DELEGATE`**
(the one verb §2.1 left out, now that there is another table to delegate *into*),
then **§2.6 the ambient-authority sweep**, which needs both.

~~**First week - four Small, near-zero-risk items closing three structural
defects.**~~ **DONE** (all four; see §1): `kernel::boot::init` deletes the three
cycles, `rheo-abi` removes the silent-corruption duplication (-943 lines),
`svc::Bridge<T>` + `NicOps`/`DisplayOps` closes the §3.2 defect, and
`TARGET-ARCHITECTURES.md` §4.1 states the `cfg` exemptions.

**Then - identity** (docs/IDENTITY.md, phases ID0-ID6). This is a **new
initiative**, not an audit finding, and it lands here because §2.1/§2.3/§2.6 are
its hard prerequisites - it is the first consumer that makes them load-bearing
rather than latent. There is currently **no permission check anywhere in the
tree** and `getuid` returns a hardcoded `1000`, so a program that drops
privileges believes it did and did not. ID1 (boot flags) is independent and can
go any time; ID2 (the kernel principal) needs §2.1; ID4 (`rwx` in the file
server) needs §2.1 + §2.6; ID5 (`identityd`) and ID6 (boot modes) come last.
**No new kernel object and no new verb** - a principal fails §6 test 2 and
`attest` already exists.

**Then - leverage.** ~~The test harness~~ **first pass DONE** (see §5: -1,533
lines, 22 kernels, no assertion touched); next: gate `VirtualLink`;
`rheo-tile-kernels`; the virtio core staged gpu → blk → net; the librheo lang-item
split (deletes `net`'s posture split permanently); `Registry`; `Skip`; the `arch`
traits; `Validated`/`Evidence`.

**Then - the substrate.** docs/SUBSTRATE.md owns the structural end of the
recurring "limit raise, not a design change" pattern this ledger keeps
recording (§4.0 and the raised caps throughout): funded per-cell kernel
metadata replacing the fixed `MAX_*` statics, the per-cell VA allocator
replacing the magic-VA map, vcores/preemption (#27), hard-float std, the
per-core timer wheel, and the workload gates - staged S1-S7 there.

**Throughout.** Every step additive per `ENGINEERING.md` §8: the pre-existing
proofs must pass **unedited**. If a proof needed editing, the step was not
additive - find out why.

---

## 7. Named but not built - the consolidated register

Everything designed, named or promised in `docs/` that has **no code behind it**, in one
list, so nothing is lost to the document it happens to live in. Each row carries the gate
that would close it and what blocks it *today*, because "not built" and "not buildable
here" are different states and conflating them is how a deferral becomes a claim.

Legend: **G** = provable in this container; **L** = needs hardware or a lab; **P** = blocked
on a prerequisite in this table.

### 7.1 The execution model (docs/EXECUTION-MODEL.md 9)

| Stage | What | Gate | State |
|---|---|---|---|
| E1 | The entity table beside the three representations | the suite unchanged | **done** (`sched/entity.rs`) |
| E6' | Host model-checker, brought forward | 7 invariants, 7 controls | **done** (`verify/entity/`) |
| E5 | A slice re-armed at every return to user | I10 on all three ISAs | **done** |
| **E2** | Ownership + the entered-guard into the table; claim and run-mark become one compare-exchange | the 8 predicate sites become 1, the 4 claim sites become 0 | **G** |
| **E3** | Runnability into the table; the personality stops keeping copies | I5 and I8, both currently unasserted anywhere | **P** (E2) |
| **E4** | Per-entity `kstack_top`, funded FP area, frame; `MAX_VCORES` deleted | the two scenarios in `verify/entity/` written *before* the implementation and deliberately without controls | **P** (E2, E3). **Unblocks: threads of one Linux cell across cores, and FA3's real producer/consumer overlap** |
| E7 | Cross-entity signal + wake IPI | a signal to an entity another core is running | **P** (E4) |
| E8 | FRED behind observation | `event_mode()` reported | **L** (see 7.3) |
| - | Migrating a **running** entity | attempted twice, reverted twice (docs/SMP.md 10.0) | **P** (E4 makes it a state-machine edge rather than a race) |
| - | Fuzzer invariants **I6, I8, I10** | each needs state E1 does not hold | **P** (E3, E4) |

### 7.2 The resource graph (docs/RESOURCE-GRAPH.md)

| What | Gate | State |
|---|---|---|
| The model, queries, per-class accessors | 11 properties, 5,000 topologies, 9 controls | **done** (`hw/graph.rs`, `verify/graph/`) |
| **Node-to-node distances**: ACPI SLIT, DT `numa-distance-map-v1` | `-numa dist,val=20` against the launch as oracle, both directions, plus the degraded single-node case | **done** - x86-64 via SLIT, riscv64 via the device tree, ARM64 degrading to `Source::None` with 0 edges asserted. Three controls firing |
| **HMAT**: real latency and bandwidth per initiator/target | `-machine q35,hmat=on -numa hmat-lb,...` | **done** - x86-64 parses the SLLBI, and the declared 100 ns / 10240 MB/s are the oracle. arm/riscv have no HMAT and assert those fields read 0 = unknown rather than a number derived from the distance |
| **Device proximity** (`_PXM`) | a driver's queues land on its device's node | **P, and the blocker is not what the earlier row implied.** `_PXM` is an **AML object**, not a table entry: ACPI has no table-based device-to-proximity mapping, so reading it needs an AML interpreter. This tree should not acquire one for one field. The routes that do not: (a) the **device tree** can put `numa-node-id` on a PCI host bridge, so riscv64/ARM64 are reachable with the walk that already exists; (b) on x86-64, derive a device's locality from its **host bridge / ECAM segment**, which is table-visible, and report `unknown` for anything behind a bridge whose proximity only AML states. Option (b) is honest and partial, which is the right shape - a device whose node is unknown must be *said* to be unknown, not defaulted to 0 |
| **Per-CPU feature divergence** (a hybrid part; cores without an FPU or a vector width their siblings have) | a CPU that lacks `FloatSimd` is asserted not to offer it | **G**. `graph_build` currently asserts the *machine's* features of every CPU and says so at the site - true of every profile this runs on, and the placeholder the row below depends on |
| **Heterogeneous-FPU handling** (docs/RESOURCE-GRAPH.md 6.4d): place hard-float work on a CPU that has an FPU, and on a trap **migrate rather than SIGILL** | FP left disabled on one secondary in software, hard-float work asserted to trap, migrate and complete natively, with the graph consulted | **P** (per-CPU discovery + **E4** for migration). The trap sites, the graph query and the resumable-fault shape all exist; the synthetic-asymmetry gate is honest about being synthetic, the `netwait` precedent |
| **LLC domains and SMT sets**: CPUID leaf `0x0B`/4, MPIDR's MT bit, DT `cpu-map` | `-smp 4,sockets=1,cores=2,threads=2` as the oracle: CPUs asserted to share a `Cache` node through `graph::siblings`, and on x86-64 to be SMT pairs on a `Core` | **done** - all three ISAs discover it (`Architectural` on x86-64/ARM64, `DeviceTree` on riscv64). The SMT half is x86-64 only, because **QEMU cannot express threads to a guest** on the other two - ARM MPIDR is index-based and riscv `cpu-map` emits no `thread` nodes, both read out of QEMU's source. Five controls firing, one of which caught a real defect: leaf 4 returns all zeros under TCG, and a first version using it alone reported two cache domains where there is one while labelling the answer discovered |
| `/sys/devices/system/{cpu,node}` synthesis | unmodified hwloc reads a correct topology | **P** (discovery) |
| `librheo::graph` read-only queries | a cell picks a lowering for an engine it cannot CPUID | **P** (discovery) |
| Driver cells publishing capabilities | a driver's queues land on its device's node | **P** (discovery + DRIVERS.md D2) |
| Memory-purpose placement (weights / KV / activations / spill / parked state) | frames land on the node the purpose names | **P** (HMAT discovery) |
| Work stealing within an LLC domain, crossings counted | a steal prefers the cache domain; the crossing count is asserted | **P** (discovery) |
| Cluster / remote `Host` nodes | one query answers local and remote alike | **P** (transport, N3b/N5a in a cell) |

### 7.3 CPU features (docs/CPU-FEATURES.md)

| What | Gate | State |
|---|---|---|
| **FRED** bring-up, probe/verify/report, IDT unchanged | `event_mode() == Fred` after one synthetic event | **L**. Checked, not assumed: QEMU 11.0.3 has FRED in `cpu.c` and `kvm/kvm.c` and **nothing in `target/i386/tcg/`**, so it is KVM-only and this container has no KVM |
| The **feature-resolution layer** (`Native`/`Translated`/`Emulated`/`Numeric`/`Unavailable`) as code | a `Numeric` translation refused under a bit-exact contract | **G** - the classification exists in `hw::graph`'s `select`; the *resolution table* (what translates to what) does not |
| **AMX** as a fourth dispatch tier | bit-exact against the scalar oracle | **L** - absent from this host and from TCG; Intel SDE unreachable under the network policy. Its fallback (int8 AMX → AVX-512/VNNI, `BitExact`) is the code running today and **is** proven |
| AVX-512 / VNNI **inside a cell, on the OS** | a cell executes it | **L** - the *kernels* are host-proven bit-exact; TCG has no AVX-512 |

### 7.4 Observability (docs/LOGGING.md 0)

| What | Gate | State |
|---|---|---|
| The console lock, always on | 210/210 | **done** |
| The per-CPU record ring with coalescing | 12 controls | **done** (`telemetry.rs`, `verify/telemetry/`) |
| **A boot that actually enables buffering** | a kernel sets `telemetry::set_buffered(true)`, drains, and asserts the transcript is whole and ordered | **G** - *nothing enables it today*, so the in-QEMU half is unexercised. The ring is proven on the host and unproven on the machine, and those are different claims |
| The metrics pipeline wired to a boot | percentiles reported from a real run | **G** - `metrics.rs` exists and no boot enables it |

### 7.5 Research adopted-in-principle (docs/GREENFIELD.md 2)

None built. Ranked there by value over cost, repeated here with gates:

| Idea | Gate | State |
|---|---|---|
| **ZNS / FDP** host-managed placement | append to a zone, the write pointer advances, an out-of-order write is refused **by the device** | **G** - QEMU models `nvme,zoned=on`. The only one provable here, which moves it up |
| **Supervision / restart** (Erlang, recovery-oriented computing) | a driver cell killed mid-request, restarted, the client completing or failing cleanly, the old fencing token refused | **G** - leases with fencing tokens already exist, which is the hard half |
| **Flow-based accounting** (Nemesis) | two clients of one driver cell, charged CPU splitting by flow rather than landing on the driver | **G** - flow context is already in the ABI |
| **Contract-checked channels** (Singularity) | an out-of-order message is a *compile* error; the wire format is unchanged | **G** - fills `idl/`, which is a stub |
| **Cross-object persistent references** (Twizzler) | a different cell traverses a persistent index at a different base with no fixup pass | **G** - PMEM grants exist |
| **Interference-driven core reallocation** (Caladan) | a latency cell's P99 held while a batch cell runs | **L** - TCG models no cache or bandwidth contention |
| **CHERI** | pointer bounds enforced by hardware | **L** - upstream QEMU has no support; Morello is hardware |
| **Accessibility / debug segments** (Arcan) | a cell's debug surface minted as an event stream | **G** |
| **A12** network-transparent display | same client semantics remote | **P** (transport) |
| **ghOSt** policy in userspace | a policy swapped without a kernel change | **P** (E2-E4 first, deliberately: a replaceable policy over three disagreeing entity representations multiplies the defect class) |

### 7.6 Open defects and unproven claims

| What | State |
|---|---|
| **QEMU 11: the 4-core GEMM barrier fails** - not all four online cores met inside one interval, where it passes on 8.2. Every phase up to two cells in user mode on two cores passes, so it is a distinct failure | **undiagnosed**, recorded in docs/SMP.md 10.1a rather than guessed at |
| ~~`/sys/devices/system/cpu/online` = `0-0` is a constant that lies~~ | **done.** The personality synthesizes `online`/`present`/`possible` from `smp::online_count()` (the `/proc/self/maps` shape), and the seeded file is gone from the disk image so a second constant cannot answer first. `cpulist` in `linuxsmp` asserts `0-3` against **QEMU's `-smp 4`**, not against the kernel's own count; the control (the constant restored) prints `online=0` and fails |
| **`/proc/stat` has one `cpuN` line whatever the boot's CPU count is** - the same defect as the row above, in the same seeded file set. Not fixed with it: the line count is cheap, but the jiffy fields need per-CPU time accounting, and a right line count with fabricated numbers beside it is not an improvement | **G** - after `sched::dispatch` charges time per CPU |
| **`linux::plock` is one big lock** over the whole dispatch. The finer per-cell locking docs/SMP.md 10.2 describes - what threads of one cell across cores would need - is not built | **P** (E4) |
| **`linux::proc::preempt_cell` is unexercised.** A 4-thread cell always has a ready sibling, so the first arm always answers; executing the second needs a single-context cell that outlives its slice | **G** - needs a fixture |
| **The full 210-boot matrix under QEMU 11** has not been run. QEMU 8.2 remains the reference emulator for every claim in this repository | **G** |
| **No wall-clock comparison with tuned Linux.** No trustworthy baseline exists in this container - a 4-hog P50 moved 1,576 ns to 22,125 ns on identical code - and TCG models no caches or TLB | **L**. The tree says "designed to, unmeasured", which is what the evidence supports |

### 7.7 The one-line summary

**Updated after distances, HMAT and CPU topology landed.** The resource graph's *discovery*
is now largely done - localities, distances, magnitudes, and which CPUs share a core or a
cache - so what remains in that section is no longer discovery but **consumers**: a steal that
prefers its cache domain and counts the crossing, memory placed by purpose, `librheo::graph`
read-only queries, `/sys/devices/system/{cpu,node}` synthesis. Two discovery rows are left and
neither is on the critical path: **per-CPU feature divergence** (the prerequisite for the
heterogeneous-FPU row, and unmodellable in QEMU beyond a synthetic asymmetry) and **device
proximity** (partial by construction, because `_PXM` is AML - see the row). Almost everything
in 7.1 is blocked on **E2**, which is a pure
refactor with the existing suite as its regression gate. Those two are the whole critical
path; the rest of this register hangs off them or off hardware.
