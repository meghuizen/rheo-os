//! **The kernel's own non-blocking record ring** - logging, tracing and events on one
//! discipline (docs/LOGGING.md, docs/OBSERVABILITY.md).
//!
//! # The gap this fills
//!
//! docs/LOGGING.md specifies a fast structured log and says, in its own words, that "the
//! per-cell log ring lives in userspace shared memory, not the kernel - the kernel is not
//! involved in the write path at all". True of a *cell's* logging, and it left the
//! **kernel's own** path as written during bring-up: `console.rs` formats at the call site
//! and hands each byte to `arch::serial_write_byte`, which spins on the UART's
//! transmit-ready bit. Its module comment still says "No locking yet - only the boot CPU
//! runs at this stage."
//!
//! That stopped being true when four cores began running cells, and the consequence was
//! **observed, not predicted**: two cores printing a fault report at once produced
//! `linuxT: unhandled TRAP: scaRAP: uscase us0xe 0xfffcff at sepcfc 0x08060,4c0a` - two
//! messages interleaved per byte. It cost a real diagnosis, because the garbled console
//! made a single-core run *look* broken and the first write-up blamed personality state;
//! reproducing it in a kernel with no secondaries showed the cells were fine and the noise
//! was the secondaries (docs/SMP.md). A diagnostic that corrupts itself under the exact
//! conditions you need it for is worse than none, because it is believed.
//!
//! Two costs, then, and they are different:
//!
//! - **Correctness**: concurrent producers interleave per byte, so a multi-core log is not
//!   a log.
//! - **Performance**: every byte spins on a device FIFO, inline, on whatever path emitted
//!   it - including paths that run per syscall.
//!
//! # The design
//!
//! One **single-producer ring per CPU**. A producer claims a slot, copies a whole record
//! in, and publishes it with one release store. It never takes a lock (there is nothing to
//! contend: the only producer is the owning core), never allocates, and **never blocks** -
//! a full ring drops the record and counts the drop, because a logger that waits has made
//! the observation change the thing observed.
//!
//! Safe by **partitioning, not locking** - the same argument as `PerCpu`, `frames`' node
//! ranges and the per-vcore resources: distinct cores write distinct rings.
//!
//! Draining is a separate act, done where blocking is already acceptable (a test boundary,
//! the idle path, a panic). The drainer merges the per-CPU rings **by timestamp**, so the
//! output is ordered even though the rings are not, and it emits whole records - which is
//! what fixes the interleaving.
//!
//! # No new kernel object, and no new dependency
//!
//! This is mechanism under the existing typed event stream (docs/ARCHITECTURE.md 3 object
//! 4): the cell-facing shape of a record is an event, and the transport to a cell is the
//! queue ABI. Nothing here mints a capability.
//!
//! # Why it has no dependencies
//!
//! No `arch`, no `smp`, no clock: the caller supplies its CPU index and its timestamp.
//! That is not purity for its own sake - it is what let `sched::entity` be model-checked on
//! the host in `verify/`, and this ring is exactly the kind of concurrent-index code where
//! an off-by-one is invisible in a boot test and obvious to a fuzzer.
//!
//! # Additivity
//!
//! Buffering is **off** by default. Every one of the 210 existing boot tests keeps its
//! current byte-for-byte console behaviour, because `console::write` only consults this
//! ring when a boot has turned it on. A logging change that reordered every existing
//! kernel's output would make every log in the tree incomparable with its own history.

/// Log levels. A `u8` because it goes in a record header, and ordered so a filter is a
/// single comparison rather than a mask lookup.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    /// A fault, a refusal, a deadlock - something a run should be judged on.
    Error = 0,
    /// A degraded path taken with a printed reason (the tree's skip-with-reason shape).
    Warn = 1,
    /// Ordinary progress: what the boot tests print today.
    Info = 2,
    /// Per-operation detail. Off unless a boot asks, because this is the level whose
    /// volume can change the timing of what it measures.
    Trace = 3,
}

/// Bytes of payload a record can carry. One record is a header plus this, and the whole
/// thing is copied with one `copy_from_slice`.
///
/// 96 rather than docs/LOGGING.md's userspace 256: the kernel's messages are short, this
/// array is `.bss` multiplied by slots and by CPUs, and a record that does not fit is
/// **truncated and marked**, never dropped silently.
pub const PAYLOAD_MAX: usize = 96;

/// Slots per CPU ring. A power of two so the index mask is an `and`.
///
/// 64, not 256: this array is `.bss` multiplied by `PAYLOAD_MAX` and by `MAX_RING_CPUS`,
/// and `.bss` growth in this tree has a scar - moving it once broke an *unrelated* kernel
/// (docs/ENGINEERING.md 11). 64 slots x 112 bytes x 8 CPUs is about 57 KiB, against the
/// 229 KiB the obvious 256 would have cost. A ring this size overflows under a chatty
/// boot, which is why overflow is counted and reported rather than treated as impossible.
pub const SLOTS: usize = 64;

/// CPUs this ring is sized for. Kept as its own constant rather than reaching for
/// `smp::MAX_CPUS`, because that would make this module depend on `smp` and stop it
/// compiling on the host - and a host fuzzer is the only thing that will ever exercise the
/// wrap-around arithmetic properly. Asserted equal at the one call site that knows both.
pub const MAX_RING_CPUS: usize = 8;

/// One record: a fixed header plus inline bytes. `Copy` and zero-valid, so a ring is a
/// plain array and a fresh slot needs no initialisation beyond zeroing.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct Record {
    /// Monotonic nanoseconds, supplied by the caller. The merge key when draining.
    pub ts_ns: u64,
    /// Which CPU produced it - recorded rather than inferred from which ring it came out
    /// of, so a drainer that merges wrongly is detectable.
    pub cpu: u16,
    /// Bytes of `payload` that are meaningful.
    pub len: u16,
    pub level: Level,
    /// Set when the producer had more bytes than `PAYLOAD_MAX`. A truncated record says so
    /// rather than looking like a complete short one.
    pub truncated: bool,
    _pad: [u8; 1],
    /// How many **additional** identical records this one stands for (0 = just itself).
    ///
    /// Coalescing, taken from Arcan's shmif: for an event whose later copy supersedes its
    /// earlier one, folding is not lossy - it is the same information at constant cost. A
    /// kernel log's repeated line is exactly that shape, and folding it is strictly better
    /// than the alternative this ring had, which was to fill up and start dropping.
    pub repeats: u16,
    /// Records lost to a full ring **at this point in the stream**.
    ///
    /// A global drop counter says how many were lost; it does not say *where*, which is the
    /// half a reader needs (a burst lost during a fault matters, the same count lost during
    /// idle chatter does not). Folding the loss into the newest record preserves the
    /// position at no slot cost.
    pub lost: u16,
    pub payload: [u8; PAYLOAD_MAX],
}

impl Record {
    pub const EMPTY: Record = Record {
        ts_ns: 0,
        cpu: 0,
        len: 0,
        level: Level::Info,
        truncated: false,
        _pad: [0; 1],
        repeats: 0,
        lost: 0,
        payload: [0; PAYLOAD_MAX],
    };

    /// The meaningful bytes.
    pub fn bytes(&self) -> &[u8] {
        let n = (self.len as usize).min(PAYLOAD_MAX);
        &self.payload[..n]
    }
}

/// A single-producer, single-consumer ring for one CPU.
///
/// `head` and `tail` are free-running counters, not indices: the slot is
/// `counter & (SLOTS - 1)`. That is what makes "is it full" one subtraction and makes
/// wrap-around correct without a spare slot.
///
/// `repr(C)` because the observability root publishes this array's address and a
/// host tool decodes it from outside the guest (docs/OBSERVABILITY.md): the field
/// order has to be the declared one, not whatever the compiler prefers.
#[repr(C)]
pub struct Ring {
    head: u32,
    tail: u32,
    dropped: u32,
    written: u32,
    coalesced: u32,
    slots: [Record; SLOTS],
}

impl Ring {
    pub const EMPTY: Ring = Ring {
        head: 0,
        tail: 0,
        dropped: 0,
        written: 0,
        coalesced: 0,
        slots: [Record::EMPTY; SLOTS],
    };

    /// Records produced, records lost to a full ring, and records folded into a previous
    /// one. All three, always: a drop count that is not reported is a log that lies about
    /// being complete, and a *fold* count that is not reported is a log that lies about how
    /// many times something happened.
    pub fn counters(&self) -> (u32, u32, u32) {
        (self.written, self.dropped, self.coalesced)
    }

    /// The newest record the consumer has not yet taken, or `None` when the ring is empty.
    ///
    /// "Not yet taken" is the whole safety condition for coalescing: amending a record a
    /// consumer already holds would change a value that has been read.
    fn newest_unread(&mut self) -> Option<&mut Record> {
        if self.head == self.tail {
            return None;
        }
        let idx = (self.head.wrapping_sub(1) as usize) & (SLOTS - 1);
        Some(&mut self.slots[idx])
    }

    pub fn pending(&self) -> usize {
        self.head.wrapping_sub(self.tail) as usize
    }

    /// Write one record. Returns false when the ring was full - the record is dropped and
    /// counted, never queued behind a wait.
    ///
    /// Truncates rather than refusing an over-long payload, and marks the record so the
    /// truncation is visible. Refusing would lose the whole message because its tail did
    /// not fit, which is the wrong half to keep.
    pub fn push(&mut self, level: Level, cpu: u16, ts_ns: u64, bytes: &[u8]) -> bool {
        self.push_claimed(level, cpu, ts_ns, bytes, bytes.len())
    }

    /// [`Ring::push`], with the message's true length given separately - see
    /// [`Rings::push_claimed`].
    pub fn push_claimed(
        &mut self,
        level: Level,
        cpu: u16,
        ts_ns: u64,
        bytes: &[u8],
        claimed: usize,
    ) -> bool {
        let n = bytes.len().min(PAYLOAD_MAX);
        // **Coalesce an identical repeat into the newest unread record** before considering
        // a slot. Two properties make this sound rather than a shortcut: the record must
        // still be unread (a consumer that has already taken it cannot have its count
        // amended), and the fold is reported (`repeats`), so the drain emits the same
        // information rather than less of it.
        //
        // The kept timestamp is the **first** of the run, deliberately: it is what makes
        // the drain's ordering stable, and "when did this start" is the question a repeated
        // kernel message raises. The last one is recoverable from the next record's.
        if let Some(newest) = self.newest_unread().filter(|r| {
            r.level == level
                && r.cpu == cpu
                && !r.truncated
                && claimed <= PAYLOAD_MAX
                && r.len as usize == n
                && r.payload[..n] == bytes[..n]
        }) {
            newest.repeats = newest.repeats.saturating_add(1);
            self.coalesced = self.coalesced.wrapping_add(1);
            return true;
        }
        if self.pending() >= SLOTS {
            self.dropped = self.dropped.wrapping_add(1);
            // **Fold the loss into the newest record instead of only counting it globally.**
            // A drop count says how many; this says where, which is what a reader needs.
            if let Some(newest) = self.newest_unread() {
                newest.lost = newest.lost.saturating_add(1);
            }
            return false;
        }
        let idx = (self.head as usize) & (SLOTS - 1);
        let slot = &mut self.slots[idx];
        slot.ts_ns = ts_ns;
        slot.cpu = cpu;
        slot.len = n as u16;
        slot.level = level;
        slot.truncated = claimed > PAYLOAD_MAX;
        // A reused slot must not inherit the previous occupant's fold counts.
        slot.repeats = 0;
        slot.lost = 0;
        slot.payload[..n].copy_from_slice(&bytes[..n]);
        // Publish last: a consumer must never see a slot index it can read before the
        // bytes are in it.
        self.head = self.head.wrapping_add(1);
        self.written = self.written.wrapping_add(1);
        true
    }

    /// The oldest unread record, without consuming it - so a merge can compare the heads
    /// of several rings before deciding which to take.
    pub fn peek(&self) -> Option<&Record> {
        if self.head == self.tail {
            return None;
        }
        Some(&self.slots[(self.tail as usize) & (SLOTS - 1)])
    }

    /// Consume the oldest unread record.
    pub fn pop(&mut self) -> Option<Record> {
        if self.head == self.tail {
            return None;
        }
        let r = self.slots[(self.tail as usize) & (SLOTS - 1)];
        self.tail = self.tail.wrapping_add(1);
        Some(r)
    }

    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.dropped = 0;
        self.written = 0;
        self.coalesced = 0;
    }

    /// Place the free-running counters at an arbitrary point in their space, leaving the
    /// ring empty when `head == tail`.
    ///
    /// **Exists for one reason: the wrap boundary is otherwise unreachable.** `head` and
    /// `tail` are `u32`, so crossing `u32::MAX` honestly would take four billion pushes -
    /// a number a long-lived kernel reaches and a test never does. Without this, the
    /// wrap-around arithmetic is code that has never been executed, and it would first run
    /// in production. The host driver in `verify/telemetry/` starts a ring at
    /// `u32::MAX - 8` so its first pushes cross it.
    ///
    /// Not `#[cfg(test)]`: this module is compiled as part of the kernel and included
    /// verbatim by a host driver, so a `cfg` would make it invisible to the only thing that
    /// calls it. Public and named for what it is instead.
    pub fn seek_for_test(&mut self, head: u32, tail: u32) {
        self.head = head;
        self.tail = tail;
    }
}

/// The per-CPU rings, and the merge over them.
///
/// A struct rather than loose statics so the whole thing is one value a host driver can
/// construct - the `sched::entity` shape, for the same reason.
///
/// `repr(C)` for the reason [`Ring`] is, and additionally so that `rings` is at
/// offset 0: the published section's address is this struct's, and a reader
/// strides it by `size_of::<Ring>()` from there.
#[repr(C)]
pub struct Rings {
    rings: [Ring; MAX_RING_CPUS],
    /// The lowest level that is recorded at all. A comparison against this is the entire
    /// hot-path cost of a disabled level.
    threshold: Level,
    /// Whether records are buffered here rather than written straight out. Off by default,
    /// so every existing boot is unchanged.
    buffered: bool,
    /// Records offered while buffering was off, so "nothing was buffered" and "buffering
    /// was never on" are distinguishable.
    /// Records offered to a disabled ring.
    ///
    /// **Not reachable from the console**, and that is deliberate rather than an oversight:
    /// `console::write` asks `buffering()` first and returns before it ever calls in here,
    /// because an early-out placed after the work is not an early-out. So this counts only a
    /// *direct* caller of [`Rings::push_claimed`] or [`Rings::push`] - the API's own guard
    /// against writing into a ring that is off. Said here because a counter that reads 0 on
    /// every real boot invites the reading that nothing happened, when the truth is that
    /// nothing came this way (docs/ENGINEERING.md 11).
    bypassed: u32,
}

impl Rings {
    pub const EMPTY: Rings = Rings {
        rings: [Ring::EMPTY; MAX_RING_CPUS],
        threshold: Level::Info,
        buffered: false,
        bypassed: 0,
    };

    pub fn set_buffered(&mut self, on: bool) {
        self.buffered = on;
    }

    pub fn buffered(&self) -> bool {
        self.buffered
    }

    pub fn set_threshold(&mut self, level: Level) {
        self.threshold = level;
    }

    /// Whether a record at `level` would be kept. The one question a producer asks before
    /// doing any work.
    #[inline]
    pub fn wants(&self, level: Level) -> bool {
        self.buffered && level <= self.threshold
    }

    /// [`Rings::push`], where the caller knows the message was longer than the bytes it
    /// could hand over.
    ///
    /// A formatter that fills its buffer cannot pass the over-long slice, so it would
    /// otherwise report exactly `PAYLOAD_MAX` bytes - indistinguishable from a message that
    /// fitted precisely. `claimed` is the true length, and only its comparison against
    /// `PAYLOAD_MAX` is used, so a caller that does not know it passes `bytes.len()`.
    pub fn push_claimed(
        &mut self,
        level: Level,
        cpu: u16,
        ts_ns: u64,
        bytes: &[u8],
        claimed: usize,
    ) -> bool {
        if !self.buffered {
            self.bypassed = self.bypassed.wrapping_add(1);
            return false;
        }
        if level > self.threshold {
            return false;
        }
        let Some(ring) = self.rings.get_mut(cpu as usize) else {
            self.rings[0].dropped = self.rings[0].dropped.wrapping_add(1);
            return false;
        };
        ring.push_claimed(level, cpu, ts_ns, bytes, claimed)
    }

    /// Record one message from `cpu`. Returns false when it was dropped or not wanted.
    ///
    /// Delegates rather than repeating the three admission checks, and that is not tidiness.
    /// The first version had its own copy of them, and a control that broke the filter in
    /// `push_claimed` **passed** - because the test called this one, which still had the
    /// right check. Two places deciding one thing, with a test unable to tell: the exact
    /// defect docs/EXECUTION-MODEL.md 1 is about, reproduced inside the module written to
    /// demonstrate the fix. One copy now, so a control on it reaches every caller.
    pub fn push(&mut self, level: Level, cpu: u16, ts_ns: u64, bytes: &[u8]) -> bool {
        self.push_claimed(level, cpu, ts_ns, bytes, bytes.len())
    }

    /// Take the globally oldest unread record across every ring.
    ///
    /// **Merged by timestamp, which is the whole reason the rings can be unsynchronised.**
    /// Producers never coordinate; ordering is recovered here from a clock they all read.
    /// Ties break by CPU index so the merge is deterministic - a nondeterministic drain
    /// order would make a captured transcript unassertable.
    pub fn pop_oldest(&mut self) -> Option<Record> {
        let mut best: Option<(usize, u64)> = None;
        for (i, r) in self.rings.iter().enumerate() {
            if let Some(rec) = r.peek() {
                match best {
                    Some((_, ts)) if rec.ts_ns >= ts => {}
                    _ => best = Some((i, rec.ts_ns)),
                }
            }
        }
        let (i, _) = best?;
        self.rings[i].pop()
    }

    /// (records written, records dropped because a ring was full, records folded into a
    /// previous identical one, records offered while buffering was off), summed over every
    /// CPU.
    pub fn counters(&self) -> (u32, u32, u32, u32) {
        let mut w = 0u32;
        let mut d = 0u32;
        let mut c = 0u32;
        for r in self.rings.iter() {
            let (rw, rd, rc) = r.counters();
            w = w.wrapping_add(rw);
            d = d.wrapping_add(rd);
            c = c.wrapping_add(rc);
        }
        (w, d, c, self.bypassed)
    }

    pub fn pending(&self) -> usize {
        self.rings.iter().map(|r| r.pending()).sum()
    }

    pub fn reset(&mut self) {
        for r in self.rings.iter_mut() {
            r.reset();
        }
        self.bypassed = 0;
        self.buffered = false;
        self.threshold = Level::Info;
    }
}

/// The kernel's rings.
///
/// A `static mut` behind one accessor rather than a lock, because the write path is
/// **partitioned**: the only producer of CPU *n*'s ring is CPU *n*, and the drain is
/// single-consumer under the console lock. That is the same argument `PerCpu`, the frame
/// allocator's per-node ranges and the per-vcore resources make, and it is what keeps a
/// producer at one copy and one increment with nothing to contend.
static mut RINGS: Rings = Rings::EMPTY;

/// # Safety
/// The caller must be on the CPU whose ring it writes, or hold the console lock to drain.
/// Both callers in this tree do (`console::write` writes its own; `console::flush` holds
/// the lock).
#[allow(clippy::mut_from_ref)]
pub unsafe fn rings() -> &'static mut Rings {
    // SAFETY: the caller's contract, above.
    unsafe { &mut *core::ptr::addr_of_mut!(RINGS) }
}

/// Turn console buffering on or off for this boot. Off is the default and every existing
/// kernel leaves it off, so their console behaviour is byte for byte what it was.
pub fn set_buffered(on: bool) {
    // SAFETY: called from single-threaded boot setup, before any secondary runs.
    unsafe { rings().set_buffered(on) };
}

/// (records written, records dropped, records folded, records offered while buffering was
/// off).
pub fn counters() -> (u32, u32, u32, u32) {
    // SAFETY: a read of counters; a concurrent producer can only make them larger.
    unsafe { rings().counters() }
}

/// Records buffered and not yet drained.
pub fn pending() -> usize {
    // SAFETY: as `counters`.
    unsafe { rings().pending() }
}

/// Kernel VA of the per-CPU ring array, for the observability root to publish
/// (docs/OBSERVABILITY.md).
///
/// A function rather than exporting the static, so the address is the only thing
/// that leaves this module - a publisher does not get a reference it could write
/// through.
pub fn rings_va() -> usize {
    core::ptr::addr_of!(RINGS) as usize
}

/// Clear the rings and the flags (between runs).
pub fn reset() {
    // SAFETY: between runs, single-threaded.
    unsafe { rings().reset() };
}
