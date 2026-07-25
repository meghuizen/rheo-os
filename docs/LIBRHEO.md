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

## Later phases (planned, not in this milestone)

B - memory & data at scale (typed grants, async I/O opcodes, a mini-DuckDB
scan). C - compute & QoS (engine/graph submit, reservations). D - the kernel's
**first interrupt** (UART RX IRQ + park-until-completion + 0%-CPU idle) and the
`term` byte-stream terminal. E - services & IPC (cross-cell connect, a
Wayland-class compositor demo). F - process/time/net + a librheo-native shell.
See the plan for the full sequence.
