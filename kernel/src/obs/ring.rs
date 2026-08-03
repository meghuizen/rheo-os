//! One CPU's event ring: the **event plane**'s storage and its wrap arithmetic
//! (docs/OBSERVABILITY.md 11).
//!
//! # Why this is its own module, and dependency-free
//!
//! Everything interesting about a ring is a wrap boundary - `head` crossing its
//! type's maximum, a slot recycled under a reader, a reader whose cursor has fallen
//! behind by more than the whole ring. A boot emits a few thousand events and
//! reaches none of them, so a boot test would pass on an implementation that is
//! wrong after four billion events, which is a number a long-lived kernel reaches.
//!
//! So this module names neither `arch` nor `smp` nor `println`: the tick and the
//! CPU index are **passed in**, and the only thing it reaches for is
//! [`crate::mm::kmeta::Funded`], which `verify/obs/fuzz.rs` shims the way
//! `verify/entity/fuzz.rs` already shims it. That is what lets the counters be
//! *started* near a boundary and crossed in milliseconds on the host. It is the
//! same shape `crate::telemetry` was written in, for the same reason and after the
//! same argument.
//!
//! # Single producer, by partitioning
//!
//! A ring belongs to one CPU, which is the only thing that ever writes it. There is
//! no lock and nothing to contend, and there is no shared sequence counter - which
//! is exactly what `crate::trace` had and what made it single-CPU by its own
//! admission. Readers are anyone, and how a reader stays correct against a live
//! writer is [`ObsRing::get`]'s whole subject.

use crate::abi::obs::{ObsEvent, ObsRingHdr};
use crate::mm::kmeta::{Funded, Owner};
use core::sync::atomic::Ordering;

/// Events held per CPU before the oldest is overwritten.
///
/// A power of two, so the slot index is a mask rather than a division.
///
/// **2048 events is 64 KiB per CPU** - 16 data frames plus a directory - taken from
/// the frame pool only when a CPU actually emits, and given back on reset. Stated as
/// a *duration* rather than only a count, because a ring depth nobody can convert
/// into time is a ring depth nobody can reason about: at a plausible one event per
/// microsecond it is about **2 ms** of history per core, and a boot that wants more
/// either narrows its windows or raises this.
pub const RING_EVENTS: usize = 2048;

/// One CPU's ring: the published header, then the storage behind it.
///
/// `repr(C)` with the header **first** because the observability root publishes this
/// array's address and a reader outside the guest strides it by
/// `size_of::<ObsRing>()`, reading a header at each step. What the kernel keeps
/// after the header is the kernel's business; the reader never needs to know, which
/// is why the frame directory's address is *in* the header rather than being
/// something a reader has to reconstruct from `Funded`'s private fields.
#[repr(C)]
pub struct ObsRing {
    /// The published half: the live counters, and where the frames are.
    pub hdr: ObsRingHdr,
    /// The event storage, funded from the frame pool and charged to the kernel.
    events: Funded<ObsEvent>,
}

impl ObsRing {
    /// An unfunded ring.
    ///
    /// `const fn` rather than a constant, because `Funded` owns frames and a
    /// duplicated value would duplicate the claim on them - the `mm::kmeta` rule.
    /// The kernel builds its array as
    /// `PerCpu::from_array([const { ObsRing::new() }; MAX_CPUS])`.
    pub const fn new() -> ObsRing {
        ObsRing {
            hdr: ObsRingHdr::new(),
            events: Funded::new(),
        }
    }

    /// Take frames for this ring and record where they landed.
    ///
    /// Returns false and changes nothing when the pool refuses, which is a clean
    /// "this CPU is not recording" rather than a boot failure: an observability
    /// facility that can take a machine down is worse than one that is absent.
    ///
    /// `phys` is supplied by the caller rather than computed here, because turning a
    /// kernel VA into a physical address is `arch`'s job and this module does not
    /// name `arch` (see the module docs).
    pub fn fund(&mut self, cpu: u32, phys: impl Fn(usize) -> usize) -> bool {
        if self.hdr.capacity != 0 {
            return true;
        }
        self.events.set_owner(Owner::KERNEL);
        if !self.events.reserve(RING_EVENTS) {
            return false;
        }
        let dir = self.events.dir_va();
        self.hdr.dir_va = dir as u64;
        self.hdr.dir_pa = phys(dir) as u64;
        self.hdr.pages = self.events.pages() as u32;
        self.hdr.per_page = crate::mm::kmeta::elems_per_page::<ObsEvent>() as u32;
        self.hdr.cpu = cpu;
        // Capacity **last**: it is the flag every other path tests, so publishing it
        // before the addresses it describes would leave a window in which a reader
        // sees a funded ring pointing at nothing.
        self.hdr.capacity = self.events.capacity().min(RING_EVENTS) as u32;
        true
    }

    /// Give the frames back and forget everything.
    pub fn release(&mut self) {
        self.hdr.capacity = 0;
        self.events.release();
        self.hdr.head.store(0, Ordering::Relaxed);
        self.hdr.unfunded.store(0, Ordering::Relaxed);
        self.hdr.dir_va = 0;
        self.hdr.dir_pa = 0;
        self.hdr.pages = 0;
    }

    /// Whether this ring can hold anything.
    #[inline]
    pub fn funded(&self) -> bool {
        self.hdr.capacity != 0
    }

    /// Events this ring can hold, or 0 if unfunded.
    #[inline]
    pub fn capacity(&self) -> u32 {
        self.hdr.capacity
    }

    /// Events written since the ring was funded, wrap included. Free-running: this
    /// is **not** how many the ring holds.
    #[inline]
    pub fn written(&self) -> u64 {
        self.hdr.head.load(Ordering::Acquire)
    }

    /// Emits offered while this CPU held no frames.
    #[inline]
    pub fn unfunded_emits(&self) -> u64 {
        self.hdr.unfunded.load(Ordering::Relaxed)
    }

    /// Events currently readable.
    #[inline]
    pub fn held(&self) -> usize {
        (self.written() as usize).min(self.hdr.capacity as usize)
    }

    /// The index of the oldest surviving event. Everything before it was
    /// overwritten.
    #[inline]
    pub fn oldest(&self) -> u64 {
        self.written().saturating_sub(self.held() as u64)
    }

    /// Append one event.
    ///
    /// The hot path, and its whole cost: one bounds test, one mask, one `Funded`
    /// directory lookup, four stores into a single 32-byte-aligned half of one cache
    /// line, and one release store of `head`. No lock, no atomic
    /// read-modify-write on the success path, no formatting, no allocation.
    ///
    /// `head` is published with **release** ordering and read with acquire, so a
    /// reader that sees the count advance also sees the record. That store is free on
    /// x86-64 (a compiler barrier), one `stlr` on ARM64 and a `fence rw,w` on
    /// RISC-V. It is paid rather than shaved because the in-guest cross-CPU reader
    /// this plane exists to serve would otherwise be reading through a data race,
    /// and the record's own sequence number - which catches a *recycled* slot -
    /// cannot catch a *half-written* one.
    #[inline]
    pub fn push(&mut self, tick: u64, window: u8, kind: u8, owner: u16, a: u64, b: u64) {
        let cap = self.hdr.capacity as u64;
        if cap == 0 {
            // Not "nothing happened": this CPU was asked to record and had no
            // memory. Counted so the two are distinguishable, which is the lesson
            // `telemetry`'s bypass counter records.
            self.hdr.unfunded.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let n = self.hdr.head.load(Ordering::Relaxed);
        let slot = (n & (cap - 1)) as usize;
        self.events.set(
            slot,
            ObsEvent {
                tick,
                a,
                b,
                seq: seq_of(n),
                owner,
                window,
                kind,
            },
        );
        self.hdr.head.store(n.wrapping_add(1), Ordering::Release);
    }

    /// Read event number `n` of this ring's stream, or `None` if it is not there.
    ///
    /// **This is where a reader's correctness lives**, so it is one function rather
    /// than an idiom every caller repeats. Two things can be wrong with a read:
    ///
    /// 1. `n` is outside `[oldest, written)` - either not written yet, or written
    ///    and already overwritten. Both are `None`, and a caller that walks from
    ///    [`ObsRing::oldest`] knows which by arithmetic.
    /// 2. `n` was inside that range when the bounds were read, and the writer
    ///    recycled the slot in the meantime. Caught by the record's own sequence
    ///    number: slot content belonging to generation `n` must carry
    ///    `seq_of(n)`, and anything else means this slot has moved on. That check
    ///    is why the sequence number is stored at all rather than being derived from
    ///    the index, which it otherwise could be.
    ///
    /// **Honest scope of check 2.** It is reasoned, not proven, and deleting it
    /// breaks nothing in the tree today - `verify/obs/fuzz.rs` was run with it
    /// removed and passed every case. That is not a fuzzer weakness but an exact
    /// statement of when the check matters: sequentially, the bounds test above
    /// already excludes every recycled slot, so the sequence number only earns its
    /// keep against a reader racing a live writer on another core. No such reader
    /// exists yet (the collector cell and the host tool are later phases), so it is
    /// kept as the thing that makes them sound rather than as something a control
    /// has demonstrated. Said plainly per docs/ENGINEERING.md 7.
    pub fn get(&self, n: u64) -> Option<ObsEvent> {
        let head = self.written();
        let cap = self.hdr.capacity as u64;
        if cap == 0 || n >= head || head.saturating_sub(n) > cap {
            return None;
        }
        let e = self.events.get((n & (cap - 1)) as usize)?;
        if e.seq != seq_of(n) {
            // The writer recycled this slot between the bounds check and the read.
            return None;
        }
        Some(e)
    }

    /// Start the counter at an arbitrary point, so the host fuzzer can reach the
    /// wrap boundary.
    ///
    /// `u64` at one event per nanosecond is not reachable in any real run, which is
    /// precisely the problem: the arithmetic around it would never be exercised by a
    /// boot, and untested arithmetic in a ring is how a long-lived kernel starts
    /// returning other events' records. Present for `verify/obs/fuzz.rs` and named
    /// so it is obvious no kernel path should call it - the same accommodation
    /// `telemetry::Ring::seek_for_test` already makes, for the same reason.
    pub fn seek_for_test(&mut self, head: u64) {
        self.hdr.head.store(head, Ordering::Release);
    }
}

impl Default for ObsRing {
    fn default() -> Self {
        Self::new()
    }
}

/// The sequence number event `n` of a stream carries.
///
/// Truncating to 32 bits costs nothing that matters: its job is to say whether a
/// slot still holds generation `n`, and a slot is recycled every `capacity` events -
/// far below `2^32` - so a stale record can never coincidentally carry the right
/// value. One-based, so that `0` in a slot means "never written" rather than
/// "written first", which matters because a funded frame arrives zeroed.
#[inline]
pub const fn seq_of(n: u64) -> u32 {
    (n.wrapping_add(1) & 0xffff_ffff) as u32
}
