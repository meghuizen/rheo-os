# LIBRHEO.md - the native userspace foundation library

librheo is the greenfield userspace foundation library for rheo-os: the role a
libc plays, rebuilt for **this** kernel. It is async-first, capability-native,
and built ON the strand runtime (`runtime/`), not a POSIX threading port. It
does **not** chase an existing ABI - that is the job of `libc/` (the C/POSIX
compat layer) and the Linux personality (docs/LINUX-COMPAT.md). librheo instead
expresses the kernel's own object model (queue pairs, capabilities, per-cell
entropy) as a Rust library a native program links.

Where the pieces sit:

- `runtime/` - the lower layer: heap, strand executor, channel, Mutex,
  type-level rights. OS-agnostic, no syscalls. librheo builds on it.
- `libc/` (rheo-libc) - the C/POSIX compatibility layer. Separate role.
- **librheo/** - the native foundation. This document.

## Charter

A native program links librheo and gets, greenfield:

- **async-first I/O** - the only way to ask the kernel (or another cell) to do
  anything is the queue pair; "blocking" exists only inside the library, as a
  strand parking on a completion token (docs/CONCURRENCY.md 1, docs/IO.md 1).
- **capability-native security** - every handle is capability-typed; widening
  rights is a compile error (docs/KERNEL-RUST.md 2). No ambient authority.
- **per-cell entropy as a library call** - a ChaCha20 DRBG in the cell, seeded
  once at startup, no syscall on the fast path (docs/TIME-IDENTITY.md 4).
- **a growable heap** so full Rust `alloc` works.

The library is heavily feature-gated in the long run (an embedded cell pulls
`cap`+`rt` with no heap; a warehouse pulls `io`+`mem`; a compositor pulls
`ipc`+`display`). Phase A ships the always-present **spine**.

## Phase A - the foundation spine (this milestone)

Phase A delivers the crate skeleton, the spine modules, the kernel surface a
loaded cell needs to actually own a queue pair, and an async round-trip that
proves it - on all three ISAs (x86-64, ARM64, RISC-V 64). The proof is the
`librhearun` test kernel: it loads `librheo-demo` into a cell **with a real
mapped queue pair + a minted capability**, and asserts the demo exits `0x42` -
which it only reaches if every async echo returned correct.

### Module map (Phase A)

| module | role |
| --- | --- |
| `mem` | `#[global_allocator]` over `runtime::Heap`, **grown on demand** by mapping more arenas from `SYS_MMAP` (the plan's grow variant, mirroring targets/std-rheo/alloc.rs). |
| `rng` | a ChaCha20 fast-key-erasure DRBG as a **library call**, seeded once at startup by drawing 32 bytes over `SYS_RANDOM`. No syscall on the fast path. |
| `cap` | capability-typed handles: re-exports `runtime::rights` (`Cap`/`Rights`/`SubsetOf`/`ReadOnly`/`ReadWrite`/`Full`) and adds `CapSet`, the bundle a cell obtains at startup (Phase A: just the queue cap). |
| `rt` | the async engine: re-exports the strand executor and adds the **userland reactor** (submit -> doorbell -> drain CQ -> `complete(user_data)`), plus `submit_and_await` and a `block_on` driver. |
| `sys` | raw syscall asm + the syscall numbers and on-wire ABI structs - a `repr(C)` duplicate of `kernel/src/abi.rs` and `kernel/src/queue/mod.rs`, kept in sync by hand (the established pattern; a cell cannot depend on the kernel crate). |
| crt0 (`start`) | `_start`: init the heap, seed the DRBG, discover the queue (`SYS_QUEUE_INFO`), build the reactor, call `main`, exit with its code. |

A librheo program is a **loaded ELF cell** built for the bare targets
(`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`,
`riscv64gc-unknown-none-elf`), like `userland/` and `libc/`. Being a
self-contained ELF, it gets `mem*` from `compiler-builtins-mem` and a heap over
`SYS_MMAP`, so the ".user heap + mem* shims" gap that blocks the fixed 2 MiB
`.user`-window cells does not apply.

### The ring ABI (on-wire queue-pair layout)

Before Phase A, a `QueuePair` kept its head/tail atomics **inside the Rust
struct**, so a separately-compiled library could not bind to it. Phase A
redefines a queue pair as an **overlay over one contiguous shared region** with
a stable `repr(C)` layout (docs/IO.md 1):

```
offset 0     QueueHeader (64 bytes, one cache line):
               u32 version        (QUEUE_ABI_VERSION = 1)
               u32 depth          (RING_DEPTH = 64)
               u32 sq_off         (byte offset of the SQ array = 64)
               u32 cq_off         (byte offset of the CQ array = 4160)
               AtomicU32 sq_head, sq_tail, cq_head, cq_tail
               u32 _reserved[8]
offset 64    SQ: [SqEntry; 64]    (SqEntry = 64 bytes)
offset 4160  CQ: [CqEntry; 64]    (CqEntry = 32 bytes)
total        8192 bytes (2 pages)
```

`SqEntry` (64 B) and `CqEntry` (32 B) are unchanged from before. The ring
indices now live **in the region**, so both endpoint overlays - the kernel's
`QueuePair` and librheo's `sys::Qp` - drive the same words over the same
physical frames, at their own virtual addresses.

The `QueuePair` type was **unified**, not forked: the same struct backs the
existing `.user`-window cells (bench-core, queue-pipeline, runtime, isolation)
and the new loaded cells. Construction changed (`init`/`attach` over a region
base instead of two separate SQ/CQ pointers); the method surface
(`sq.push`/`cq.pop`/`submit`/`reap`/`kernel_process`) is the same, so the four
pre-existing queue tests kept passing. Two constructors:

- `QueuePair::init(base)` - write a fresh header and overlay (identity-mapped
  `.user` cells, host tests: kernel VA == user VA).
- `QueuePair::init_header(base)` + `QueuePair::attach(user_va)` - the loader
  writes the header through the kernel linear map, then binds the overlay at
  the cell's VA, since PA != VA for a loaded cell.

`submit` also gained a variant carrying up to 24 bytes of opcode arguments in
`payload` (`submit_args`), `reap` returns the full `CqEntry` (status, result,
`user_data` = the strand token, flow context), and `SYS_DOORBELL` now returns
the number of entries processed. `OP_ECHO` reads a `u32` from `payload[0..4]`
and returns it in the completion's `result` - the null round-trip with a data
touch that the async proof checks.

### The reactor model

A strand "blocks" by parking on a token (`runtime::strand::park_on(token)`);
the token is a submission's `user_data`. The **reactor** is the cell's side of
its queue pair:

1. `submit(op, args, token)` writes a submission stamped with the token.
2. `block_on` runs every ready strand until they have all parked, then
   **pumps**: rings `SYS_DOORBELL` (the kernel drains the SQ, grant-checks each
   entry, executes, and writes completions), drains the CQ, and calls
   `complete(user_data)` for each - waking exactly the strands whose ops
   finished. One wakeup, N strands resumed.
3. `submit_and_await(op, args).await` ties it together: allocate a token,
   submit, park, and return the completion.

This is the docs/CONCURRENCY.md 1 model run from **userspace** (the `runtime`
test kernel proves the same loop in kernel context). There is still no
completion interrupt anywhere in the kernel (no IRQ path exists yet -
docs/LIBRHEO.md Phase D), so a reactor with nothing ready would spin; in Phase
A the driver always has work to pump, and a true 0%-CPU idle waits for the
kernel's first interrupt.

### Kernel surface added (Phase A)

All additions **extend the existing queue object** or **expose existing
mechanism** - none is a new kernel object (they pass the docs/ARCHITECTURE.md 6
admission rule as mechanism/exposure):

- **`SYS_QUEUE_INFO` (31)** - `queue_info(out_va) -> 0 | u64::MAX`. Writes a
  `QueueInfo { qp_va, cap_id }` at `out_va`: the base VA of the cell's mapped
  queue region and the 32-bit ABI id of its minted `QueuePair` capability. The
  reactor calls it once at startup. A native, explicit alternative to an auxv
  entry.
- **`load::map_queue`** - maps a fresh queue region into a loaded cell at
  `USER_QUEUE_VA` (16 GiB, above image/stack/mmap), writes its header through
  the kernel linear map, and returns an overlay bound to the user VA. The
  caller mints the `QueuePair` capability (READ|WRITE) and records
  `(qp_va, cap_id)` with `user::set_queue_info`.
- **`SYS_DOORBELL`** - now returns the processed count; for a loaded cell it
  drains the cell's real mapped `QueuePair` (reachable during the trap because
  the cell's address space is active).
- **`runtime::Heap::add_region`** - arena growth, so a heap over `SYS_MMAP` can
  map more backing memory on demand (the grow logic, which must call a syscall,
  stays in librheo; the allocator stays OS-agnostic).

The VA convention for a loaded cell is now: image 1-4 GiB, stack 8 GiB, anon
mmap 12 GiB and up, **queue region 16 GiB** (`load::USER_QUEUE_VA`).

## Phase B - memory & data at scale (this milestone)

Phase B makes librheo the substrate for terabytes-of-data / analytical-DB /
warehouse workloads: **real typed memory** and **real async bulk I/O**, proven
by a **zero-copy columnar scan off the live virtio-blk disk** on all three ISAs.
The proof is the `librheodata` test kernel: a librheo cell reads a columnar
dataset off a live disk, runs a mini-DuckDB scan across strands, and asserts the
exact aggregate.

### Kernel surface added (Phase B)

All additions **expose an existing kernel object** or **extend the queue
object** - none is a new object (they pass the docs/ARCHITECTURE.md 6 admission
rule as mechanism / exposure). Typed memory grants are per-cell state (a fixed
static table, like the Linux fd table); each grant also mints a real MemoryGrant
capability, and every commit/decommit/seal is grant-checked (MAP right).

Typed memory-grant syscalls (expose object 5, docs/MEMORY.md):

- **`SYS_GRANT` (32)** - `grant(out_va, len, kind, flags) -> 0 | u64::MAX`.
  Reserves `len` bytes of typed address space (no frames - demand commit), mints
  a MemoryGrant capability, and writes a `GrantInfo { base, cap_id }`. `kind` is
  a `MemKind` (DDR/HBM/CXL/PMEM/DeviceBar/Remote). **Only DDR is real in QEMU**;
  HBM/CXL/PMEM/Remote are **backed by DDR frames** (emulated, honest); DeviceBar
  has no backing and is **refused**. Reservations are pure 48-bit address space,
  so a multi-GiB grant costs nothing until committed.
- **`SYS_COMMIT` (33)** / **`SYS_DECOMMIT` (34)** - back / unback a sub-range of
  a grant with frames (demand paging **without a fault handler** - explicit
  commit; generalizes the L4 `mprotect`-commit path to a native syscall).
- **`SYS_SEAL` (35)** - make a grant immutable (its committed pages become
  read-only, shareable) - the zero-copy-buffer / dmabuf precursor (Phase E IPC).
- **`SYS_MUNMAP` (36)** - real unmap for native cells: frees the frames of
  `[va, va+len)`. Fixes the anon-`SYS_MMAP` frame leak (that path had a global,
  never-freed cursor and no unmap). The anon VA *cursor* stays monotonic (a
  benign, documented address-space-only leak at 48-bit VA scale).
- **`SYS_MMAP_FILE` (37)** - `mmap_file(fd, offset, len, flags) -> base VA`.
  Maps a file range into the cell (read into fresh frames, MAP_PRIVATE, mapped
  read-only) - the substrate for mmap-ing a dataset. Reads through the same
  `svc::FileOps`/VFS the POSIX personality uses.

Real async I/O opcodes over the queue (extend the queue object, docs/IO.md 1):

- **`OP_OPEN`/`OP_READ`/`OP_WRITE`/`OP_CLOSE`/`OP_FSTAT` (2-6)** - each reads its
  arguments from the `SqEntry.payload`, performs the op via `svc::FileOps`, and
  pushes a `CqEntry` carrying `user_data` (the strand token). The Phase A
  file/console I/O was a *separate synchronous fd path*; these bridge it to
  `kernel_process` so it completes through the completion ring - the reactor's
  strand-park-on-completion model now covers real I/O.
- **Per-opcode rights** - reads require READ, mutating ops require WRITE (the
  hardcoded-WRITE of Phase A is gone; a narrowed read-only queue cap would now
  enforce it).
- **Distinct completion statuses** - each `CapError` gets its own status
  (`STATUS_BAD_HANDLE`/`REVOKED`/`EXHAUSTED`/`DENIED`), not a collapsed deny, so
  a cell can tell why an op was refused.
- **Contract fields on the submission** (docs/IO.md): `SqEntry.flags` carries an
  inline-vs-by-reference bit (`FLAG_INLINE`) and a durability class. **Inline vs
  by-reference threshold**: a write at or below `INLINE_MAX` (16 bytes) rides in
  the submission payload; a larger read/write is **by reference** at a buffer /
  grant VA. Above the threshold, I/O is **zero-copy**: because the submitting
  cell's address space is active during its `SYS_DOORBELL` trap, the read/write
  lands directly in the cell's mapped grant pages - no kernel bounce. Durability
  (`FLAG_DUR_FLUSH`/`FUA`) and the latency window are **advisory today** (QEMU
  has no durable / real-time backend); they are recorded on the op, honored
  best-effort, and documented as such.

The native-cell VA map gains: file mmap **20 GiB** (`FILEMMAP_BASE`), grant
reservations **32 GiB** (`GRANT_BASE`), above image (1-4), stack (8), anon mmap
(12+), and queue (16).

### librheo modules (Phase B)

| module | role |
| --- | --- |
| `mem` (extended) | `Grant` (typed, `reserve`/`commit`/`decommit`/`seal`, RAII), `Arena` (bump over a committed grant), `Mapping` (a file mmap), and a NUMA-hint API (`reserve_on(kind, len, node)` - single-node in QEMU, honest). |
| `io` (new) | async `File` (`open`/`read_at`/`write_at`/`close`/`size` over the OP_* opcodes + reactor), `read_into` (zero-copy read straight into a `Grant`), batched submit (N ops, one doorbell, await all), a `Contract` (durability class + latency window), and a `Stream`. |
| `store` (new, thin) | async dataset access over `io` + `mem` for the bulk path - `Dataset::open`/`map_all`/`map`. Folded together with `io` for now (the block/object transport underneath is the kernel's `BlockDevice`/virtio-blk seam; documented). |

### The proof: the mini-DuckDB scan (`librheodata`)

The dataset is a raw columnar blob - a 16-byte header then column A
(`col_a[i] = i`) then column B (`col_b[i] = i & 1`), 65536 rows x 2 u32 columns
(~512 KiB) - **generated fresh by xtask into `target/` (never committed)** and
attached to the `librheodata` kernel as a **live virtio-blk disk** (virtio-mmio
on arm/riscv `virt`, virtio-pci on x86 q35, exactly like `blockfs`). The kernel
reads the whole disk off the live device and serves it to the librheo cell
through a single-file `FileOps` (`/data.col`), so both the async-I/O path and
the mmap path reach the real disk bytes.

The librheo cell then exercises the whole Phase B surface and exits `0x42` only
if it all passed and the aggregate is exact:

1. **typed grants** - reserve+commit a DDR grant, write/read a pattern,
   decommit+recommit a page (demand paging), seal it (a later commit is
   refused), request an emulated HBM grant (succeeds), confirm a device-BAR
   grant is refused.
2. **async I/O** - open the dataset, `fstat` its size, async-read the 16-byte
   header into a committed grant (each an OP_* submission parked on a
   completion), parse `nrows`/`ncols`.
3. **batched async read** - N=8 strands async-read the 8 partitions of column A
   into a grant concurrently; **one doorbell drains all 8 completions**, landing
   straight in the grant (zero-copy read into a grant). The async-read column
   must match the mmap'd column.
4. **zero-copy mmap scan** - mmap the dataset, fan the columnar scan across N=8
   strands (each partition computes a partial `SUM`/`COUNT`/`MAX` of `col_a`
   where `col_b == 1` over mapped memory - **no syscall per access**), reduce,
   and assert the exact closed-form aggregate (for 65536 rows:
   `SUM = 1073741824`, `COUNT = 32768`, `MAX = 65535`).
5. an inline `OP_WRITE` console marker (the sub-threshold write path).

Zero-copy is **real**: the mmap scan touches mapped memory with no syscall per
access, and OP_READ / OP_MMAP_FILE land bytes directly in the cell's mapped
frames with no kernel bounce buffer. The one unavoidable copy is the
filesystem's backing store -> the mapped frames, done once at map/read time (the
FS owns those bytes). Promoting an open fd to a **first-class file capability**
(`ObjectKind::File`, added but not yet wired) is a documented next step; today
fds remain `svc::FileOps` handles carried in the payload.

Honest accounting: HBM/CXL/PMEM/Remote grants are emulated on DDR; NUMA is
single-node; durability/latency contracts are advisory (no durable/RT backend in
QEMU); a first-class file capability and a real block/object `store` transport
are deferred.

## Phase C - compute & QoS (this milestone)

Phase C makes librheo the substrate for **parallel / accelerated compute with
QoS guarantees** (DuckDB parallel+SIMD, ML, warehouse rollups): strands as M:N
parallel workers, **userspace-built dependency graphs submitted to the compute
engine**, engine introspection, and **admission-checked reservations**. The
proof is the `librheocompute` test kernel: a librheo cell runs a parallel
aggregation, submits a real graph to the CPU engine, admits and rejects
reservations, and reports the engine's measured throughput - on all three ISAs.

### Kernel surface added (Phase C)

All additions **expose an existing kernel object** or **extend the queue
object** - none is a new object (they pass the docs/ARCHITECTURE.md 6 admission
rule as mechanism / exposure of objects 4/6/7).

Graph submission over the queue (extends the queue object, docs/IO.md 1):

- **`OP_GRAPH_SUBMIT` (7)** - a queue opcode (so it rides the async model and
  the Phase A reactor, completing with the strand token). Payload
  `[nodes_va u64][count u32][results_va u64]`: `count` `abi::GraphNode`s live in
  one of the cell's own buffers. The kernel reads them (the cell's address space
  is active during the `SYS_DOORBELL` drain, so the VAs are directly readable -
  no bounce), builds a `graph::Graph`, **validates the edges** (an input may only
  reference an earlier node - topological), runs it on the CPU engine
  (`kernel/src/engine.rs`), and writes each node's `u64` result back to
  `results_va`. The completion carries the node count. A malformed edge / empty /
  oversized graph completes with a distinct status (not a fault). Node ops are the
  arithmetic set `graph.rs` already has (Const/Add/Mul/Select - a conditional edge
  for MoE routing / speculative decoding); a **buffer-reduce / map node kind** (a
  graph node that reduces a mapped buffer) is a documented next step - it needs
  object 6's node model to carry buffer references, a larger change. The parallel
  *aggregation* is served today by the strand `map_reduce` path below.

Engine introspection + reservations (expose objects 4 / 7):

- **`SYS_ENGINE_INFO` (38)** - `engine_info(out_va) -> 0`. Writes an
  `EngineInfo { kind, measured_cost_ticks, preemption }`: the CPU engine's kind
  (0 = CPU), the throughput **measured at attach** (attest-by-measurement, object
  4 - the engine benchmarks a known op stream in `Engine::attach` and records the
  per-op cost; it is a measurement, never a vendor claim), and its preemption
  contract. Answers librheo `compute`'s "what executor am I on?".
- **`SYS_RESERVE_ADMIT` (39)** - `reserve_admit(out_va, budget, period, deadline,
  mem_floor_pages) -> 0 | 1=BadParams | 2=Overcommit | 3=MemoryFloor`. Runs the
  **EDF schedulability test** (`sched::Admission`, object 7): a per-cell admission
  controller tracks committed utilization (`sum(budget_i/period_i) <= 1`, integer
  parts-per-million) and refuses a set it cannot guarantee. On success it mints a
  **Reservation capability** (`ObjectKind::Reservation`, READ) into the cell's
  table - the reservation the cell holds - and writes a `ReserveInfo { handle,
  committed_ppm }`. A refused reservation returns a code **cleanly, never faults**.
  The memory floor is an advisory check against the current free-frame pool
  (QEMU has no bandwidth/IO backend, so CPU is the real guarantee).
- **`SYS_RESERVE_QUERY` (40)** - the cell's committed CPU utilization (ppm).
- **`SYS_RESERVE_RELEASE` (41)** - `reserve_release(cap_id) -> 0 | u64::MAX`.
  Grant-checks the Reservation capability, returns its utilization to the
  admission controller, and frees the slot (the RAII drop path).

**Honest accounting - admission is real, enforcement is SMP-gated.** The
admission **math** (EDF utilization, over-commit refusal, memory-floor check) is
real and enforced *at admit time* - an over-committed set is refused, exactly
like a real-time system that says no rather than accepting a lie. But the runtime
is single-CPU **cooperative** today, so a reservation is an *admitted guarantee*,
not yet a *scheduled* one: actual run-queue enforcement (a reserved cell getting
its budget, priority-driven preemption) lands with SMP/preemption (task #27).
The **CPU engine is the only real engine**; GPU/NPU accelerators run behind the
same graph/engine API as attested-firmware future work (docs/ACCELERATORS.md).
Priority bands are carried but advisory.

### librheo modules (Phase C)

| module | role |
| --- | --- |
| `compute` (new) | **strands as M:N parallel workers** - `map_reduce` (partitioned map + reduce, the parallel aggregation), `parallel_for` (disjoint-block loop), `scan` (blocked parallel prefix sum), all over the Phase A executor; **engine introspection** (`Engine::info` -> kind + measured throughput + preemption); and **`GraphBuilder`** - build a dependency graph (`constant`/`add`/`mul`/`select`) in userspace and `submit().await` it to the CPU engine over `OP_GRAPH_SUBMIT`, reading back the result. SIMD-friendly aligned buffers come from `mem` (`Grant`/`Arena`). |
| `sched` (new) | **`Reservation`** (`request(budget, period, deadline, mem_floor)` -> an admitted, RAII handle or a typed `ReserveError::{BadParams,Overcommit,MemoryFloor}`); the `lattice-rt`-shaped surface - `Priority`, a `PeriodicTask` builder whose `.build()` runs admission, and a `TimingReport` (committed utilization + headroom). |

On the single-CPU cooperative runtime the parallel strands **interleave** rather
than run on separate cores (SMP work-stealing is task #27); the surface is the
parallel *decomposition*, and every aggregate is exact.

### The proof: parallel compute + graph + reservations (`librheocompute`)

The `librheocompute` test kernel loads `librheo-compute` into a cell with a real
mapped queue pair + minted capability (like `librhearun`), attaches/measures the
CPU engine (`svc::init`), and asserts the cell exits `0x42` - which it reaches
only if every stage passed:

1. **parallel aggregation** - `compute::map_reduce` fans a columnar
   `SUM WHERE odd` over an in-memory dataset (`col[i] = i`) across 8 strands and
   asserts the exact closed-form value (`SUM = (LEN/2)^2 = 4194304`). `scan` and
   `parallel_for` are also verified.
2. **graph submission** - `GraphBuilder` builds `n0=const(6); n1=n0+1; n2=n1*n0`
   and submits it to the CPU engine over the async queue; the result `42` is
   asserted (userspace-built graph, run in the kernel, completed through the ring).
3. **reservations** - a feasible reservation (30% CPU) is admitted (committed
   ppm > 0); an infeasible one (`budget > period`) is cleanly rejected
   (`BadParams`); an over-commit (another 80% on the held 30%) is rejected
   (`Overcommit`); the `PeriodicTask`/`TimingReport` builder path is exercised,
   and RAII release returns the CPU to fully uncommitted.
4. **engine info** - the engine kind + measured throughput are printed (visible,
   not asserted-exact).

## Phase D - the kernel's first interrupt + the terminal (this milestone)

Phase D delivers the OS's **first block-and-wake**: a native cell with nothing to
do parks, and the kernel idles until console input arrives instead of spinning.
On top of it librheo grows **`term`**, the byte-stream terminal discipline a
bash-quality shell is built on. The proof is the `librheoterm` test kernel: a cell
runs an interactive read-eval loop over `term`, driven by scripted keystrokes that
exercise editing, history, and an escape sequence, and exits `0x42` only if every
committed line is exact - on all three ISAs.

### Kernel surface added (Phase D)

- **`SYS_WAIT_INPUT` (42)** - `wait_input(buf_va, len) -> nbytes`. **Blocks**
  until at least one console input byte is available, copies up to `len` bytes
  into the cell buffer, and returns the count (0 = end of input). The first
  block-and-wake: the kernel idles here (WFI where the UART RX interrupt is wired,
  a poll otherwise). This is **mechanism** (the arm-doorbell / completion-delivery
  the plan names), not a new kernel object - it passes the docs/ARCHITECTURE.md 6
  admission rule.
- **a kernel-side RX ring** (`kernel/src/input.rs`, portable) - received bytes are
  buffered here, so a keystroke typed while a cell computes is not lost (the
  16-byte UART FIFO is the only other buffer). The producer is the UART RX
  interrupt handler where one is wired, or the poll path; the consumer is
  `wait_input`. A scripted source (headless tests) feeds the same path.

The reactor (`rt`) grows a **console-read** slot: `rt::read_console(buf, len)`
registers a request and parks the strand; `block_on`, when no queue completion is
ready, services it by blocking in `SYS_WAIT_INPUT` - the terminal idle path. One
reader at a time (a terminal has a single input stream). While the reader is
parked the vcore runs the other strands; only when they have all parked does the
kernel block for a byte. This is the docs/CONCURRENCY.md 1 park-on-a-token model
finally closed by a real wakeup.

### The interrupt path, per ISA (honest)

This is the kernel's first hardware interrupt, and it is boot-critical, so it was
brought up one ISA at a time behind an explicit, opt-in enable
(`arch::enable_uart_rx_irq`, called only by the Phase D test) - the other 26 test
kernels never touch it and run exactly as before.

- **RISC-V 64 - interrupt-driven.** QEMU `virt` with `aia=aplic-imsic` routes the
  16550 UART's IRQ (source 10) through the **AIA**: the S-mode APLIC
  (`aplic@d000000`, MSI-delivery mode) raises an MSI into the S-mode IMSIC
  (`imsics@28000000`, one interrupt file per hart, reached through the indirect
  CSRs `siselect`/`sireg`/`stopei`), which drives `sip.SEIP`. The kernel enables
  the UART's RX-data interrupt (IER bit 0), configures the S-APLIC (source 10
  delegated by M-mode, level-high, MSI target = this hart + EIID 10) and the
  S-IMSIC (`eidelivery`/`eithreshold`/`eie`), and takes the supervisor external
  interrupt (`scause` = interrupt | 9) in the kernel trap handler, draining the
  UART into the RX ring. Cells run with `sstatus.SIE` clear (a U-mode SEI would
  look like a fault); the kernel sets SIE only briefly, after `wfi` woke on a
  pending SEI, to service it - so `SYS_WAIT_INPUT` is a **genuine 0%-CPU park**.
  The `librheoterm` test asserts the kernel actually idled at `wfi`
  (`input::did_idle`). *QEMU caveat, documented for honesty:* QEMU's 16550
  loopback (used to inject a deterministic scripted keystroke into the RX FIFO)
  delivers the byte to the receiver but does **not** drive the APLIC's input line,
  so the test raises the UART's MSI in the IMSIC directly - which is exactly the
  MSI the configured S-APLIC would send. The byte is genuinely received, a genuine
  S external interrupt genuinely fires, and the CPU genuinely halts at `wfi` until
  it does; the full UART->APLIC->IMSIC wiring is configured but not exercised
  end-to-end by loopback in this harness.
- **x86-64 - poll.** The IOAPIC RTE for ISA IRQ4 + LAPIC + the 16550 IER path is
  the documented next step; today `SYS_WAIT_INPUT` polls COM1 (the CPU spins -
  honest, not 0%-idle). `input::interrupt_driven()` reports false.
- **ARM64 - poll.** The GICv3 (GICD/GICR) + PL011 `IMSC.RXIM` path is the
  documented next step; today `SYS_WAIT_INPUT` polls the PL011.

`input::interrupt_driven()` reports which mode an ISA is in; the test asserts the
idle-park only where it is interrupt-driven, and prints the mode either way.

### librheo `term` (Phase D)

| module | role |
| --- | --- |
| `term::input` | a decoder turning the raw console byte stream into typed [`Key`]s - CSI (`ESC [ ...`) and SS3 (`ESC O ...`) escape sequences (arrows, Home/End/Delete/Insert/PageUp-Down, F1..F12), UTF-8 codepoints, control chars - with an async `next_key().await` that parks on the input completion (`rt::read_console`). Escape/keymap decoding is userland, never kernel (docs/SHELL.md 1). |
| `term::edit` | a line editor over a `Vec<char>` buffer: insertion, cursor movement (left/right/home/end), backspace/delete, word-kill (^W) and line-kill (^U/^K), history recall (up/down with a stashed fresh line), and a completion hook (Tab) - the reedline/rustyline-class core. Returns an `Edit` (`Commit`/`Redraw`/`Eof`/`Noop`). |
| `term::render` | a buffered, minimal-diff renderer: batched writes (one buffer, one `write` - not a syscall per byte), line repaint with erase-to-EOL (CSI K) so a shorter line clears leftovers, and absolute cursor positioning. ANSI escapes emitted in userland. |

Raw mode is the default (no kernel echo or line discipline - librheo owns it); the
byte stream (`rt::read_console`) is the primary API, with the typed `Key` layer on
top. Typed `KeyEvent`/control channels (SHELL.md 1) remain an optional later layer.

### The proof: `librheoterm`

`librheo-term` runs the read-eval loop over `term` and is fed the scripted
keystrokes `"worlq"<BS>"d"<CR>` -> `world`, `"helo"<Left>"l"<CR>` -> `hello`,
`<Up><Up><CR>` -> `world` (recall the older history entry). It exercises typing,
backspace, cursor-left + insert, an escape sequence (arrows), and history recall,
verifies the three committed lines exactly, and exits `0x42`. The test asserts the
exit code on all three ISAs, and on RISC-V additionally asserts the kernel idled at
`wfi` (interrupt-driven), printing the input mode (interrupt-driven / poll) either
way.

Honest accounting: RISC-V is interrupt-driven (with the loopback/APLIC caveat
above); x86-64 and ARM64 are on the poll path (their interrupt-controller bring-up
- IOAPIC/LAPIC, GICv3+PL011 - is the documented next step). The cooperative,
single-CPU model is unchanged: this is "wake on input", not preemptive scheduling
(SMP/preemption is task #27).

## Phase E - services & IPC, a Wayland-class compositor (this milestone)

Phase E makes librheo the substrate for **services and a Wayland-class
compositor**: two cells that share a **typed cross-cell queue pair** and pass
**ownership of a sealed buffer grant** (zero-copy), with a **flip/present
completion**. The proof is the `librheowl` test kernel: a client cell and a
compositor cell exchange a sealed buffer over a shared queue pair; the client
"commits" a frame, the compositor composites it and returns a flip completion,
and the client asserts the compositor's checksum equals its own known value -
proving zero-copy cross-cell sharing - on all three ISAs.

This closes docs/IO.md 6 ("Cross-cell calls are the same ABI: connect =
capability exchange yielding a typed queue pair") and the compositor primitives
of docs/GRAPHICS.md / docs/DISPLAY.md (a surface + a present/flip queue; buffer
grants = the dmabuf equivalent; a HID event stream).

### Kernel surface added (Phase E)

Both additions **expose an existing kernel object** - none is a new object (they
pass the docs/ARCHITECTURE.md 6 admission rule as mechanism / exposure of
objects 2/3/5). A cross-cell queue pair is the queue object (3) mapped into two
cells; buffer passing is capability delegation (2) of a sealed grant (5).

- **`SYS_CONNECT` (43)** - `connect_info(out_va) -> 0 | u64::MAX`. Writes a
  `ChannelInfo { chan_va, cap_id, role }`: the base VA of the cell's mapped
  **shared-channel** region, its minted QueuePair capability id, and its role (0
  = initiator/client, 1 = acceptor/server) so one binary serves both ends. A
  cross-cell channel is **one ring region mapped into two cells** at the same
  channel VA (`load::USER_CHANNEL_VA`, 24 GiB); the kernel writes the ring header
  once (`load::alloc_channel` + `map_channel_into`) but **never drains it** - the
  two cells drive the SPSC rings directly over the shared frames (the client is
  the SQ producer + CQ consumer, the server the reverse), so a message is a pure
  shared-memory write, not a kernel round trip. This is the IO.md-6 typed queue
  pair whose two ends live in two cells.
- **`SYS_GRANT_SHARE` (44)** - `grant_share(grant_cap_id, out_va) -> 0 |
  u64::MAX`. **Delegate a sealed grant to the peer cell** (`cur ^ 1`): the kernel
  maps the grant's frames into the peer **read-only** at the peer's next grant VA
  (`AddressSpace::share_ro_into`, portable over the existing per-ISA leaf walk),
  mints a MemoryGrant capability there referencing the **same** kernel object (so
  an epoch revoke on the client's object kills the peer's copy too - revocable),
  and writes a `ShareInfo { peer_va, peer_cap_id }`. It requires the grant
  capability to carry **DELEGATE** (object 2) and the grant to be **sealed**
  (object 5 seal -> immutable = shareable). This is **zero-copy shared memory**:
  the same physical frames back both cells, so the compositor reads the client's
  buffer with no copy (the dmabuf equivalent). `SYS_GRANT` now mints grants with
  `READ|WRITE|MAP|DELEGATE` so a cell can share its own buffers.

The native-cell VA map gains the **shared channel at 24 GiB**
(`load::USER_CHANNEL_VA`), between the file mmap (20 GiB) and grant (32 GiB)
regions.

### librheo modules (Phase E)

| module | role |
| --- | --- |
| `ipc` (new) | `Channel` - a typed cross-cell queue pair over the shared ring (`open` discovers this cell's end + role via `SYS_CONNECT`; client `send`/`await_completion`, server `recv`/`complete`, both over `SqEntry`/`CqEntry`), the cooperative `switch_to_peer` hand-off, and **buffer passing**: `share(&Grant) -> SharedBuffer` (delegate a sealed grant to the peer) + `recv_buffer(peer_va, len)` (view the delegated frames read-only, zero-copy). |
| `display` (new) | the compositor primitives: `Surface` (a client drawable backed by a sealed buffer grant; `commit` seals + delegates the buffer, sends the handle + geometry, and awaits the flip completion), `Compositor` (an in-memory framebuffer; `present` receives a frame, composites the shared buffer, and replies with the flip completion), and `InputEvent` (the Phase D `term::input::Key`, delivered over the same typed channel - the HID event-stream shape). |

### The proof: the compositor demo (`librheowl`)

`librheo-wl` is **one binary run as two cells** (the test kernel installs both,
gives each its own kernel queue pair, wires one shared channel into both, and
sets roles - like the harness wires `.user` cells; Phase E proves the connect
mechanism, spawn-driven connect is Phase F). Which cell is which is decided by
the role `SYS_CONNECT` reports (cell 0 = client, cell 1 = compositor):

1. **client** - allocates a 64x64 buffer grant, draws a known pattern into it,
   computes its checksum, then `commit`s: **seals** the buffer, **shares** it to
   the compositor (`SYS_GRANT_SHARE` - the same frames mapped read-only into the
   compositor), sends the buffer handle + geometry over the shared channel, and
   awaits the flip completion.
2. **compositor** - receives the buffer handle over the channel, maps the sealed
   grant read-only (**zero-copy** - the same frames), composites it into its own
   framebuffer (the one compositing copy), checksums the framebuffer, and sends
   the **flip/present completion** back over the channel carrying that checksum.
3. **client** - reaps the flip completion (the frame callback) and asserts the
   compositor's checksum **equals its own known value** - which it can only do if
   the compositor read the exact client bytes over the shared mapping - then
   exits `0x42`.

The test asserts the client exits `0x42` on all three ISAs. Because the
compositor's checksum matches the client's (observed identical -
`0x3eb4f800` for the demo pattern), the buffer share is genuinely **zero-copy**:
the compositor reads the client's frames directly, no bounce buffer, no copy of
the client's bytes into the kernel or the compositor's own memory (the single
copy is the compositor's *own* composite into its framebuffer, exactly as a real
server blends a surface into the scanout).

### Honest accounting (Phase E)

- **Zero-copy is real.** `SYS_GRANT_SHARE` maps the client's sealed frames into
  the compositor; the shared queue pair is shared memory the kernel never
  touches. The only copy is the compositor's deliberate composite into its
  framebuffer.
- **The channel is synchronous with an explicit peer hand-off.** On the
  single-CPU cooperative runtime a producer submits then `switch`es so the
  consumer runs (and vice-versa). Folding the channel into the strand reactor as
  a park-on-channel-completion (a fully symmetric async `Sender<T>`/`Receiver<T>`)
  is the documented refinement; the mechanism (typed shared queue pair + sealed
  buffer + flip completion) is the deliverable.
- **Spawn-driven connect is Phase F.** Here the test kernel wires the two cells
  and their shared channel; a cell spawning its peer and exchanging the channel
  capability at runtime waits for native `SYS_SPAWN` (Phase F).
- **Real GPU is deferred** (docs/DISPLAY.md). The framebuffer is in-memory, not a
  virtio-gpu scanout. The compositor mechanism - shared sealed buffer, typed
  present queue, flip completion, input-event shape - is the deliverable; a real
  display engine is future work.
- **Single frame, single buffer.** A `Surface` commits once; double-buffering
  with a per-frame buffer pool is a documented refinement.

## Later phases (planned, not in this milestone)

F - process/time/net + a librheo-native shell. See the plan for the full
sequence.
