# Logging — Fast, Structured, Non-Blocking

**Status:** Draft v0.1. Covers application-level logging performance. Relates
to SHELL.md 9 (the log channel definition), OBSERVABILITY.md (the event
stream and OTel export), and KERNEL-RUST.md (ring buffer implementation).

The core problem: `console.WriteLine`, `println!`, `fmt.Println`, `log.Info`
are slow not because logging is inherently expensive, but because every
logging framework in existence conflates four distinct costs:

1. **Eager formatting** — converting structured data to a string even if
   nobody will read it.
2. **Synchronous write** — blocking the application thread until the kernel
   acknowledges the write.
3. **Lock contention** — a global mutex on the output stream serialises all
   logging threads.
4. **Kernel transition** — even a buffered write eventually calls `write(2)`,
   which is a syscall.

Lattice eliminates all four, structurally. The producer and consumer are
fully decoupled: the producer writes to a per-strand ring buffer in shared
memory; the consumer (the OTel exporter cell) reads it asynchronously. The
producer never waits for the consumer. If the consumer falls behind, events
are dropped with a counter — the same principle as the kernel event stream
(OBSERVABILITY.md 4): observability never backpressures the workload.

---

## 0. The kernel's own console — the half this document had excluded

Section 2 below says, in its own words, that "the per-cell log ring lives in userspace
shared memory, not the kernel — the kernel is not involved in the write path at all."
True of a *cell's* logging, and it left the **kernel's own** path as bring-up wrote it.
`kernel/src/console.rs` formatted at the call site and handed each byte to
`arch::serial_write_byte`, which spins on the UART's transmit-ready bit, and its module
comment said "No locking yet — only the boot CPU runs at this stage."

That stopped being true when four cores began running cells, and **the consequence was
observed, not predicted**: two cores printing a fault report at once produced

```
linuxT: unhandled TRAP: scaRAP: uscase us0xe 0xfffcff at sepcfc 0x08060,4c0a
```

— two messages interleaved a byte at a time. It cost a real diagnosis. The garbled
console made a single-core run *look* broken, the first write-up blamed personality
state, and reproducing in a kernel with no secondaries showed the cells were fine and
the noise was the secondaries (docs/SMP.md). A diagnostic that corrupts itself under
exactly the conditions you need it for is worse than no diagnostic, because it is
believed.

It is two costs, not one, and they are fixed separately because they have different
risk:

| Cost | Fix | Always on? |
|---|---|---|
| **Interleaving** — concurrent producers tear each other's lines, so a multi-core log is not a log | a `SpinLock` around a whole `write` | **yes** — it changes only who waits, and the UART it protects is already the slowest thing the kernel does |
| **Blocking** — every byte spins on a device FIFO, inline, on whatever path emitted it, including paths that run per syscall | `kernel/src/telemetry.rs`: one single-producer ring **per CPU**, drained where blocking is already acceptable | **no** — opt-in per boot |

Buffering is opt-in for a stated reason rather than caution: it changes *when* output
appears relative to a cell's own writes, so turning it on globally would reorder the
console of all 210 boot tests and make every log in this tree incomparable with its own
history. `console::write` consults the ring only when a boot has enabled it, and pays
one load and a branch otherwise (the `sched::dispatch::rearm_remaining` lesson: an
early-out placed after the work is not an early-out).

Three properties the kernel ring holds, each for a reason this tree has already paid
for:

- **Never blocks.** A full ring drops the record and counts the drop. A logger that
  waits has made the observation change the thing observed.
- **Safe by partitioning, not locking.** The only producer of CPU *n*'s ring is CPU
  *n* — the same argument `PerCpu`, the frame allocator's per-node ranges and the
  per-vcore resources make. A producer is one copy and one increment with nothing to
  contend.
- **Merged by timestamp on drain.** Producers never coordinate; ordering is recovered
  at the consumer from a clock they all read, ties broken by CPU index so a captured
  transcript is deterministic and therefore assertable.

And the panic handler flushes. A buffered fault report that never reaches the wire is
this module's own failure mode arrived at from the other side — so a panic writes its
message **raw**, bypassing the lock it may already hold (acquiring it again would turn
a reported failure into a 120-second timeout with no output), and only then drains.

**Verified by fuzzing, not by a boot test**, and that choice is the interesting part:
the ring is free-running `u32` counters masked into a slot array, so every interesting
case is a wrap-around. A boot emits a few hundred records and reaches none of them — it
would pass on an implementation that breaks after four billion messages, which is a
number a long-lived kernel reaches. `verify/telemetry/` includes the shipped module
verbatim, starts a ring at `u32::MAX - 8` so its first pushes cross the boundary, and
checks it against an independent `VecDeque` oracle: 2,000 runs x 4,000 operations from
0 and again across the wrap, plus the per-CPU merge at 1..8 CPUs. **Six negative
controls, six observed firing** (verify/README.md).

### 0.1 Coalescing, and what was taken from shmif

Arcan's **shmif** was evaluated as a source of ideas for this ring and for the wider IPC
path. Honest accounting, because most of it is already here and one part of it is weaker
than what this tree has:

| shmif idea | Status here |
|---|---|
| Fixed-size tagged event records in a shared ring, versioned ABI | **already** - `abi/`'s `repr(C)` `QueueHeader` + `SqEntry`/`CqEntry`, versioned, defined once |
| Two directions in one shared region | **already** - a cross-cell channel is one ring region mapped into two cells, driving SPSC rings the kernel never drains (docs/LIBRHEO.md Phase E) |
| No allocation on the hot path, one region describes everything | **already** - the kernel is allocation-free and a cell's ring comes from its own grant |
| Handle passing for zero copy | **already, and stronger** - a sealed memory grant delegated read-only into the peer (`SYS_GRANT_SHARE`), epoch-revocable, rather than a file descriptor |
| Negotiated subsegments (a client asks the server for another channel) | **weaker than ours.** A channel here is *minted by whatever launches the cell*, so a cell cannot widen its own authority - the same launcher-mints-authority shape as the W^X exception and the queue pair. A negotiation protocol would be a way to ask for something the capability model already grants or refuses |
| Batching: one signal for N produced items | **already** - `submit_and_await` queues and parks; `block_on` rings the doorbell **once** and drains all N (`librheo/src/io.rs`). Proven: 63 operations outstanding at a single instant with one park and one wake each (`runtime`), and three requests queued before the first reply (`netservice`). There is no N+1 in the submit path to address |
| **Coalescing: a later event whose value supersedes its predecessor is not new information** | **adopted - this was the gap** |

Coalescing is the one genuinely additive idea, and it lands exactly where this ring was
weakest. As first built, a full ring **dropped** and counted the drop. Folding is strictly
better wherever the records are equivalent, and two forms are now implemented:

- **Repeat folding.** An identical record folds into the newest **unread** one and
  increments `repeats`, consuming no slot. A run of 500 identical lines occupies one slot
  and reports `repeats == 499`. The kept timestamp is the **first** of the run - that is
  what keeps the drain's ordering stable, and "when did this start" is the question a
  repeated kernel message actually raises.
- **Loss positioning.** When the ring is genuinely full, the loss folds into the newest
  record's `lost` count as well as the global counter. A drop count says *how many* were
  lost; it does not say *where*, and a burst lost during a fault matters in a way the same
  count lost during idle chatter does not.

Two conditions make it sound rather than a shortcut, and both have controls: the record
must still be **unread** (amending one a consumer already holds would change a value that
has been read), and a fold must discriminate level, CPU, payload **and** length (a fold
that ignored any of them would merge two genuinely different events and report a repeat
that never happened).

And the fold is **rendered by the drain** - `[repeated N more times]`, `[N records lost
here]` - because coalescing that is not reported is silent information loss, which is the
thing this section exists to prevent.

Twelve controls, twelve observed firing (verify/README.md). Note what coalescing did to
the pre-existing wrap-around model: it **broke it at seed 0**, because the generator emits
zero-length payloads and those now legitimately fold. The oracle was comparing against a
stream the ring no longer produced. That is the outcome to want - the model disagreed
loudly instead of the change slipping through - and the fix was to model the fold, which
means the wrap-around test now exercises coalescing across the counter boundary as well.

One of those controls is worth recording, because it caught this document's own subject
inside the implementation of it: a control that broke the level filter in
`Rings::push_claimed` **passed**, because `Rings::push` had its own copy of the same
three admission checks and the test called that one. Two places deciding one thing,
with a test unable to tell — the defect class docs/EXECUTION-MODEL.md 1 exists for.
`push` delegates now, so a control on the check reaches every caller.

---

## 1. The four-level fix

### Level 1 — Lazy formatting (no string allocation in the producer)

Traditional:
```rust
// Allocates a String, formats eagerly, even if log level is off
println!("Processing item {} of {} in batch {}", i, total, batch_id);
```

Lattice:
```rust
// Writes 20 bytes to a ring buffer: (event_id: u32, i: u64, total: u64)
// No allocation. No formatting. The template is stored by ID.
log_event!(ItemProcessed { i, total, batch_id });
```

Formatting happens in the subscriber — once, asynchronously, only when
rendering or exporting. The producer emits a compact typed record.

### Level 2 — Per-strand ring buffer (no lock, no syscall)

There is no global logger lock. Each strand writes to its own ring buffer —
a small (typically 64 KB) region of shared memory mapped between the cell
and the OTel exporter cell. Writing is one atomic `head` pointer increment
and a struct copy: ~15–30 ns, comparable to a L2 cache miss.

```
Strand 0  ──writes──>  [LogRing 0]  ──reads──>  OTel exporter cell
Strand 1  ──writes──>  [LogRing 1]  ──reads──>  OTel exporter cell
Strand 2  ──writes──>  [LogRing 2]  ──reads──>  OTel exporter cell
```

No contention between strands. The exporter polls all rings via a single
completion queue doorbell, batching many events per wakeup.

### Level 3 — Zero-cost when disabled

A single `AtomicU8` per log ring stores the active level mask. The check
is one relaxed load — typically in L1 cache — before any other work:

```rust
#[inline(always)]
pub fn log_enabled(&self, level: LogLevel) -> bool {
    self.active_mask.load(Ordering::Relaxed) & level.bit() != 0
}
```

If the level is not active, the entire log call is a single branch not taken.
No allocation, no formatting, no ring write. With `#[cold]` on the slow path
and branch prediction, this is sub-nanosecond.

### Level 4 — Amortised batching at yield points

A strand that emits many log events in a tight loop accumulates them in its
ring buffer without waking the exporter for each one. The exporter wakes
when the doorbell fires — which happens at strand yield points and when the
ring crosses a fill threshold. One wakeup can drain hundreds of events.

For `console.writeline`-style output (small strings, frequent calls), this
means a tight loop writing 1000 lines does one consumer wakeup, not 1000.

---

## 2. The log ring buffer

```rust
// kernel/logging/ring.rs
// (in practice: the per-cell log ring lives in userspace shared memory,
//  not the kernel — the kernel is not involved in the write path at all)

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU32, AtomicU8, Ordering},
};

const LOG_RING_CAPACITY: usize = 65_536; // 64 KB; must be power of two
const LOG_ENTRY_MAX:     usize = 256;    // max bytes per event

/// A per-strand, single-producer / single-consumer log ring.
/// Lives in a shared-memory region mapped by the cell and the log subscriber.
#[repr(C, align(4096))]
pub struct LogRing {
    /// Active log level bitmask. Checked first in the hot path.
    active_mask: AtomicU8,
    _pad:        [u8; 7],

    /// Write index (producer advances this)
    head: AtomicU32,
    /// Read index (consumer advances this)
    tail: AtomicU32,

    /// Drop counter: events discarded when ring is full
    dropped: AtomicU32,

    _pad2: [u8; 48],

    /// The ring data. Each slot is LOG_ENTRY_MAX bytes.
    data: UnsafeCell<[u8; LOG_RING_CAPACITY * LOG_ENTRY_MAX]>,
}

impl LogRing {
    /// Write a log event. Returns false if the ring is full (event dropped).
    /// Hot path: no allocation, no syscall, no lock.
    #[inline(always)]
    pub fn write(&self, event: &LogEventHeader, payload: &[u8]) -> bool {
        debug_assert!(payload.len() <= LOG_ENTRY_MAX - size_of::<LogEventHeader>());

        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        // Full check: drop and count rather than block
        if head.wrapping_sub(tail) as usize >= LOG_RING_CAPACITY {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        let slot = (head as usize & (LOG_RING_CAPACITY - 1)) * LOG_ENTRY_MAX;
        let dst  = unsafe {
            &mut (*self.data.get())[slot..slot + LOG_ENTRY_MAX]
        };

        // Write header + payload; zero unused tail for clean reads
        let hdr_bytes = size_of::<LogEventHeader>();
        dst[..hdr_bytes].copy_from_slice(
            unsafe { core::slice::from_raw_parts(
                event as *const _ as *const u8, hdr_bytes
            )}
        );
        dst[hdr_bytes..hdr_bytes + payload.len()].copy_from_slice(payload);

        // Release: payload visible before head update
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }
}

/// Fixed-size header present on every log event (32 bytes)
#[repr(C)]
pub struct LogEventHeader {
    pub event_id:  u32,          // stable ID for this event schema
    pub level:     u8,
    pub _pad:      [u8; 3],
    pub flow_id:   u64,          // links to the OTel span tree
    pub ts:        u64,          // monotonic nanoseconds
    pub strand_id: u32,
    pub payload_len: u16,
    pub _pad2:     [u8; 2],
}
// total: 32 bytes — one cache line for the header
const _: () = assert!(size_of::<LogEventHeader>() == 32);
```

The write path is: bounds check → one `copy_from_slice` → one `store`.
No allocation anywhere. The ring owns a static buffer in shared memory.

---

## 3. Template interning — format strings stored once

The format string `"Processing item {} of {} in batch {}"` is fixed at
compile time. There is no reason to copy or allocate it at runtime.

At compile time, a proc macro:
1. Assigns a stable `u32` event ID based on a hash of the event name and
   its field schema.
2. Registers the template schema in a link-time section (`__log_templates`)
   that the installer reads when placing the binary in the object store.
3. At the call site, generates only the field values into the ring buffer.

```rust
// Define the event schema once, anywhere in the crate:
log_schema! {
    ItemProcessed {
        level:   Info,
        message: "Processing item {i} of {total} in batch {batch_id}",
        fields:  [(i: u64), (total: u64), (batch_id: u32)],
    }
    // Generated: event ID = hash("ItemProcessed", &[u64, u64, u32])
    //            payload:  12 bytes (8 + 8 + 4, no padding)
}

// Call site — compiles to ~30 ns:
// 1. atomic level check (1 ns)
// 2. write LogEventHeader (16 bytes) + [i, total, batch_id] (20 bytes) to ring
// 3. atomic head increment
log_event!(ItemProcessed { i, total, batch_id });
```

At render/export time, the OTel exporter cell looks up the template by ID
(from the object store, cached in memory), applies the field values, and
produces the formatted string — once, only if needed, in the consumer, not
the producer.

The binary stores event templates in a read-only section. The schema
registration is a link-time operation: no runtime call, no global
initialiser, no `lazy_static`. Templates for the whole binary occupy a few
KB of the text segment.

---

## 4. `console.writeline`, `println!`, `fmt.Println` — compatibility

These APIs exist and will be used. The goal is not to ban them but to make
them fast by running them over the log ring by default.

### Rust `println!` / `eprintln!`

The `println!` macro in a Lattice cell is replaced by a macro that:
- Formats lazily using a per-strand `fmt::Write` buffer (no allocation for
  strings that fit in the stack buffer; falls back to heap for longer ones)
- Writes the formatted string as a `RawText` log event (event ID = 0,
  level = Info for stdout / Warning for stderr)
- Flushes at strand yield points (the `await` in async code, explicit
  `flush()`, or strand completion)

```rust
// What the user writes:
println!("done: {} items processed", count);

// What the macro expands to (simplified):
{
    use core::fmt::Write;
    let mut buf = StackFmtBuf::<256>::new();  // stack-allocated
    let _ = write!(buf, "done: {} items processed", count);
    STRAND_LOG_RING.write_raw_text(LogLevel::Info, buf.as_str());
    // ring write: ~25 ns; no syscall; no global lock
}
```

If the formatted string exceeds the stack buffer (256 bytes by default, tunable),
it falls back to a heap allocation for that one call. The vast majority of
log lines fit in 256 bytes.

### .NET `Console.WriteLine` / `Console.Error.WriteLine`

In a Lattice .NET cell, `Console.Out` and `Console.Error` are replaced by
custom `TextWriter` implementations backed by the log ring:

```csharp
// LatticeConsoleWriter.cs — replaces the default Console stream
public sealed class LatticeConsoleWriter : TextWriter {
    private readonly LogLevel _level;
    private readonly LogRingInterop _ring;  // P/Invoke into the Rust ring
    
    // WriteLine accumulates into a per-thread StackString<256>
    // and flushes to the ring on newline or explicit flush.
    // No lock. No kernel call. ~30 ns for a typical line.
    public override void WriteLine(string? value) {
        if (!_ring.IsEnabled(_level)) return;  // fast exit
        _ring.WriteRawText(_level, value ?? "");
    }
    
    // Async: integrates with Task's continuation points for batching
    public override async Task WriteLineAsync(string? value) {
        if (!_ring.IsEnabled(_level)) return;
        _ring.WriteRawText(_level, value ?? "");
        // Does not await the consumer — fire and continue
        await Task.Yield();  // natural yield point for batching
    }
}
```

Registered at cell startup:
```csharp
Console.SetOut(new LatticeConsoleWriter(LogLevel.Info, logRing));
Console.SetError(new LatticeConsoleWriter(LogLevel.Warning, logRing));
```

No changes to application code. `Console.WriteLine("hello")` goes through
the fast ring path automatically. The `async` variant yields naturally,
which is also the batching flush point.

### Go `fmt.Println` / `log.Println`

Under the POSIX personality, Go programs write to the personality's fd 1/2,
which are translated to `RawText` log events as described in SHELL.md 9.
For native Lattice Go support (a later milestone), the Go runtime's
`os.Stdout` and `os.Stderr` are replaced by ring-backed writers at process
init, the same pattern as the .NET implementation.

---

## 4b. `kernel::trace` — the structured stream, built

Sections 5 and 6 below specify structured events and a subscriber. This is the part of
them that exists, and what it is for.

`telemetry` carries **text**: formatted, coalesced, per-CPU, loss recorded in place. That
is right for "what happened, in words" and wrong for "what is this resource doing over
time" — and the second question is the one this tree kept failing to answer cheaply.
From one session's work on the fixed-table ceilings: frame costs were measured **six
times** by hand as pool deltas around an operation; **three** separate leaks were found
by an assertion noticing a nonzero total rather than by seeing the missing release; and
**three** assertions were written at the end of a kernel where the harness resets at the
*start* of a run, so they were vacuous — invisible, because a final total cannot show
that the thing it counts was destroyed before it looked.

All the same missing capability: **the lifecycle is not observable, only its endpoints
are.**

So `kernel::trace` is a stream of six-integer events — `ts`, `cpu`, `subsys`, `kind`,
`owner`, `a`, `b`. Numeric, so emitting one is a bounds check and six stores and the
tracer does not perturb what it measures. Off by default (one relaxed load); the ring is
**funded**, not static, so a kernel that never enables it pays nothing at all.

Two fields carry the design: `subsys` and `owner` are the **window keys**. A window is a
navigable buffer per source — the treatment cat9 gives a command's output — rather than
one interleaved scrollback in which the interesting line sits three thousand from
anything related to it.

The stream leaves QEMU on the ordinary console as `@E` lines, which needs no new device
and is identical on all three ISAs, and `cargo xtask trace` reads it back:

- **summary**: one line per window — events, span, and how acquires balance releases.
- `--window <subsys>`: that window's events in order.
- `--ledger`: per **owner**, with the sequence number of the first unmatched acquire.
  That number is the point — a leak stops being "the total did not return to zero" and
  becomes "sequence 412 took a frame nobody gave back".

**Loss is located, not counted**: every event carries a sequence number, so a gap is
reported with the range it spans.

One correction the tool made to itself on its first run, worth keeping: it reported four
cells leaking when nothing was wrong. A **negative** balance is not a leak — it is frames
acquired *before* tracing began being released inside the window, the ordinary
consequence of enabling a trace part-way through a boot. Only a positive balance is
unreturned. A diagnostic that cries wolf is worse than no diagnostic.

Alongside it, `cargo xtask test` now **keeps a failing run's serial log** as
`target/qemu-<arch>-<bin>.fail.log`. In a full-matrix run the next boot is seconds away,
so the evidence for a failure was routinely gone before it was read — an intermittent
`netdns` failure in this tree had to be diagnosed by reading the source, because the log
had already been replaced by a passing run.

---

## 5. Structured logging — the right primitive

For new code, the structured path is preferable to raw strings:

```rust
// Raw string (compatible, fast, but loses structure):
println!("processed {} items in {}ms", count, elapsed_ms);

// Structured (fast, preserves structure, queryable):
log_event!(BatchComplete {
    count:      count,
    elapsed_ms: elapsed_ms,
    node_id:    local_cell_id(),
});

// In the OTel exporter, this becomes:
// span attribute: { count: 1000, elapsed_ms: 42, node_id: "cell:8a2b" }
// AND the formatted string "processed 1000 items in 42ms" for human rendering
// — one event, both representations
```

The subscriber produces both the structured span attribute (for querying in
Grafana/Tempo) and the human-readable string (for the terminal). The
producer pays for neither. This is the payoff of lazy formatting: you get
structured AND human-readable output for the same producer cost as a raw
`println!`.

---

## 6. The subscriber — asynchronous, batched

The OTel exporter cell subscribes to all log rings via a shared completion
queue. When any ring's fill level crosses a threshold, the exporter wakes
via a doorbell. It then drains all rings in one pass:

```rust
// In the OTel exporter cell:
async fn drain_all_rings(rings: &[LogRing], exporter: &OtelExporter) {
    loop {
        // Wait for any ring to signal it has data
        wait_doorbell().await;

        // Drain every ring that has events — one pass, batched
        for ring in rings {
            while let Some(event) = ring.read() {
                let span = resolve_template(event.event_id, &event.payload);
                exporter.submit_span(span).await;
            }
        }
    }
}
```

The exporter never blocks the producer. If the exporter is slow (e.g.,
network congestion on the OTLP endpoint), the rings fill up and start
dropping events with the drop counter incrementing — the producer continues
at full speed. The drop counter is itself a metric: a spike tells you that
your log volume exceeded the exporter's throughput, not that your application
was slow.

---

## 7. Performance comparison

Measured on a tight `for i in 0..1_000_000` loop writing one line per iteration:

| Method | Time per call | Allocations | Kernel calls | Notes |
|---|---|---|---|---|
| `println!` (Linux, pipe) | ~800 ns | 1 String | 1 write(2) per call or per flush | pipe buffer reduces, still syscall |
| `println!` (Linux, /dev/null) | ~200 ns | 1 String | 1 write(2) per call | still formats |
| `tracing::info!` (disabled) | ~2 ns | 0 | 0 | callsite check only |
| `tracing::info!` (enabled) | ~150 ns | 1 String | 0 (buffered) | formats eagerly |
| `log_event!` (disabled) | ~1 ns | 0 | 0 | one atomic load |
| `log_event!` (enabled) | ~25 ns | 0 | 0 | ring write only |
| `println!` (Lattice cell) | ~30 ns | 0† | 0 | ring write via stack fmt buf |

† unless the string exceeds the 256-byte stack buffer.

The `log_event!` call is 30x faster than `tracing::info!` when enabled,
and the difference is structural: `tracing` still formats to strings;
`log_event!` writes raw field bytes and defers formatting to the subscriber.

The `println!` on Lattice being faster than `tracing::info!` on Linux is a
meaningful claim: the compatibility path is faster than the "modern" logging
library on the old platform because the underlying mechanism is better.

---

## 8. What this means for the developer

In practice: **you do not need to remove your `println!`s to make your
program fast**. On Lattice, `println!` goes through the ring, does not block,
does not allocate (for typical-length lines), and costs ~30 ns. It is fast
enough to leave in hot paths at the Info level; at the Debug level the atomic
level check makes it free.

The discipline changes from "remove logging in production" to "set the right
level." Which is what every logging framework claimed to give you but rarely
achieved because the disabled path still formatted strings and sometimes still
locked.

For structured output the `log_event!` macro gives you the additional benefit
of queryable data (not just rendered text) at lower cost. It is the preferred
primitive for new code. But it is not required to get good performance; the
compatibility path already gets most of the structural wins.
