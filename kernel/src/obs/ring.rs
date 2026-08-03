//! One CPU's event ring: the **event plane**'s storage and its wrap arithmetic
//! (docs/OBSERVABILITY.md 11).
//!
//! # The storage is one contiguous block, and that is an architecture decision
//!
//! The first version stored events in `mm::kmeta::Funded` pages and paid a
//! page-split - a shift, a mask and a dependent directory load - **per recorded
//! event**, because the frame allocator had no contiguous path. The pool grew one
//! (`frames::alloc_contig`, which the GICv3 ITS work had already asked for and gone
//! without), and the ring became `base + slot * 32` with nothing between the slot
//! arithmetic and the stores. It also simplified everything downstream: the
//! published header carries one base address instead of a directory to walk, a host
//! reader's job is a linear read, and this module now depends on **nothing** - the
//! tick, the CPU index, the memory and the physical address all arrive from the
//! caller.
//!
//! # Why the wrap is checked on the host
//!
//! Everything interesting about a ring is a boundary - the sequence number's 32-bit
//! recycle, a slot reused under a reader, a reader whose cursor has fallen behind by
//! more than the whole ring. A boot emits a few thousand events and reaches none of
//! them, so a boot test would pass on an implementation that is wrong after four
//! billion events, which is a number a long-lived kernel reaches.
//! `verify/obs/fuzz.rs` drives this file verbatim with the counters *started* near
//! each boundary; being dependency-free is what makes that a `#[path]` include
//! rather than a model.
//!
//! # Single producer, by partitioning - and that includes interrupts
//!
//! A ring belongs to one CPU, which is the only thing that ever writes it. There is
//! no lock and nothing to contend, and there is no shared sequence counter - which
//! is exactly what `crate::trace` had and what made it single-CPU by its own
//! admission. Readers are anyone, and how a reader stays correct against a live
//! writer is [`ObsRing::get`]'s whole subject.
//!
//! One constraint is inherited from the rest of the kernel and stated here so it is
//! not rediscovered as a corruption: **an emit must not interrupt an emit**, even on
//! one CPU. [`ObsRing::push_packed`] is load-head, store-fields, store-head, and a
//! handler emitting between those steps would write the same slot. It cannot happen
//! today - cells run with interrupts masked and the kernel takes interrupts only in
//! its idle paths, none of which sit inside an emit - which is why this plane does
//! not pay the local-cmpxchg reserve/commit protocol Linux's ring buffer pays on
//! every event for exactly this (there, anything can trace inside anything, up to
//! and including NMIs). A design that lets a handler interrupt kernel-context code
//! must revisit this paragraph.

use crate::abi::obs::{ObsEvent, ObsRingHdr};
use core::sync::atomic::Ordering;

/// Events held per CPU before the oldest is overwritten.
///
/// A power of two, so the slot index is a mask rather than a division.
///
/// **2048 events is 64 KiB per CPU** - 16 contiguous frames - taken from the frame
/// pool only when a CPU is asked to record, and given back on reset. Stated as a
/// *duration* rather than only a count, because a ring depth nobody can convert
/// into time is a ring depth nobody can reason about: at a plausible one event per
/// microsecond it is about **2 ms** of history per core, and a boot that wants more
/// either narrows its windows or raises this.
pub const RING_EVENTS: usize = 2048;

/// Frames per ring, for whoever allocates and frees the block.
pub const RING_PAGES: usize = RING_EVENTS * size_of::<ObsEvent>() / 4096;

/// One CPU's ring. Nothing but the published header: the storage is the block
/// `hdr.base_va` names, owned by this ring between [`ObsRing::fund`] and
/// [`ObsRing::release`] but allocated and freed by the caller, which is what keeps
/// this module dependency-free.
///
/// `repr(C)` with the header **first** because the observability root publishes this
/// array's address and a reader outside the guest strides it by
/// `size_of::<ObsRing>()`, reading a header at each step.
#[repr(C)]
pub struct ObsRing {
    /// The published half - which, with contiguous storage, is the whole of it.
    pub hdr: ObsRingHdr,
}

/// u64 words per record, for the whole-word stores in [`ObsRing::push_packed`].
const WORDS: usize = size_of::<ObsEvent>() / 8;

// `push_packed` writes the record as four u64s, so the words it writes and the
// fields a reader decodes must be the same bytes. Asserted against the real layout
// rather than trusted to a comment: if a field moves, this fails to compile instead
// of every record silently carrying its neighbours' values.
const _: () = assert!(WORDS == 4);
// The packed word's field order assumes little-endian, which all three ISAs are.
// Asserted so a port to a big-endian machine fails here, at the assumption, rather
// than by every reader decoding swapped fields.
const _: () = assert!(cfg!(target_endian = "little"));
const _: () = assert!(core::mem::offset_of!(ObsEvent, tick) == 0);
const _: () = assert!(core::mem::offset_of!(ObsEvent, a) == 8);
const _: () = assert!(core::mem::offset_of!(ObsEvent, b) == 16);
const _: () = assert!(core::mem::offset_of!(ObsEvent, seq) == 24);
const _: () = assert!(core::mem::offset_of!(ObsEvent, owner) == 28);
const _: () = assert!(core::mem::offset_of!(ObsEvent, window) == 30);
const _: () = assert!(core::mem::offset_of!(ObsEvent, kind) == 31);
const _: () = assert!(RING_EVENTS.is_power_of_two());

/// Pack the constant-shaped half of a record - owner, window, kind - into the one
/// word it occupies in [`ObsEvent`]'s last eight bytes (little-endian: `seq` low,
/// then `owner`, `window`, `kind`).
///
/// This is the ftrace lesson applied at the ABI: `window` and `kind` are
/// compile-time constants at every call site, so packing there lets the constant
/// half **fold into one immediate** - a call site with a constant owner passes the
/// whole word as a single `movabs` where it used to spend three register moves, and
/// the ring stores one u64 where it used to store four sub-word fields. `const fn`
/// so exactly that folding happens.
#[inline(always)]
pub const fn pack_meta(window: u8, kind: u8, owner: u16) -> u64 {
    ((kind as u64) << 56) | ((window as u64) << 48) | ((owner as u64) << 32)
}

impl ObsRing {
    /// An unfunded ring.
    pub const fn new() -> ObsRing {
        ObsRing {
            hdr: ObsRingHdr::new(),
        }
    }

    /// Adopt `base_va` - [`RING_PAGES`] contiguous, zeroed frames the caller
    /// allocated - as this ring's storage, and publish where it is.
    ///
    /// The block must be **zeroed**, because an untouched slot is recognised by its
    /// zero sequence number ([`seq_of`] is one-based for exactly that), and it must
    /// be physically contiguous, because `phys(base_va)` is published once and a
    /// reader reaches every record by offset from it.
    ///
    /// `phys` is supplied by the caller rather than computed here, because turning a
    /// kernel VA into a physical address is `arch`'s job and this module names
    /// nothing (see the module docs).
    pub fn fund(&mut self, cpu: u32, base_va: usize, phys: impl Fn(usize) -> usize) {
        if self.hdr.capacity != 0 || base_va == 0 {
            return;
        }
        // `ObsEvent` is align(32) - the never-straddles-a-cache-line property - so a
        // block that is not is a caller bug, not a degraded mode. Free in a kernel
        // build (frames are page-aligned), live in the host fuzzer, where it fired
        // on a `Vec<u64>` block the first time this was driven there.
        debug_assert!(
            base_va.is_multiple_of(align_of::<ObsEvent>()),
            "ring block at {base_va:#x} is not {}-aligned",
            align_of::<ObsEvent>()
        );
        self.hdr.base_va = base_va as u64;
        self.hdr.base_pa = phys(base_va) as u64;
        self.hdr.cpu = cpu;
        // Capacity **last**: it is the flag every other path tests, so publishing it
        // before the address it describes would leave a window in which a reader
        // sees a funded ring pointing at nothing.
        self.hdr.capacity = RING_EVENTS as u32;
    }

    /// Forget the storage and hand it back to the caller to free.
    ///
    /// Returns the block's kernel VA, or 0 if the ring was never funded. The caller
    /// frees the frames because the caller allocated them; this split is what keeps
    /// the module dependency-free, and it makes "every fund path is a release path"
    /// a property the caller's one reset function owns.
    pub fn release(&mut self) -> usize {
        let va = self.hdr.base_va as usize;
        // Capacity first, so no push can reach the block after it goes back.
        self.hdr.capacity = 0;
        self.hdr.base_va = 0;
        self.hdr.base_pa = 0;
        self.hdr.head.store(0, Ordering::Relaxed);
        self.hdr.unfunded.store(0, Ordering::Relaxed);
        va
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

    /// Append one event, unpacked. The compatibility spelling of
    /// [`ObsRing::push_packed`], for callers holding the fields loose.
    #[inline]
    pub fn push(&mut self, tick: u64, window: u8, kind: u8, owner: u16, a: u64, b: u64) {
        self.push_packed(tick, pack_meta(window, kind, owner), a, b);
    }

    /// Append one event whose owner/window/kind half is already packed
    /// ([`pack_meta`]).
    ///
    /// The hot path, written the way ftrace writes a record: **in place, as whole
    /// words**. Four u64 stores - tick, `a`, `b`, and the packed word with the
    /// sequence number or-ed into its low half - where the first version built an
    /// [`ObsEvent`] and let the compiler copy it field by field, seven stores, four
    /// of them sub-word. On ARM64 the four pair into two `stp`s. With the storage
    /// contiguous there is no page split either: the address is `base + slot * 32`,
    /// one load for the base. Measured from the disassembly, not estimated - the
    /// whole function is ~22 instructions on x86-64 with no prologue at all.
    ///
    /// No lock, no atomic read-modify-write on the success path, no formatting, no
    /// allocation. `head` is published with **release** ordering and read with
    /// acquire, so a reader that sees the count advance also sees the record - free
    /// on x86-64, one `stlr` on ARM64, a fence on RISC-V, and paid because the
    /// cross-CPU reader this plane exists to serve would otherwise read through a
    /// data race.
    #[inline]
    pub fn push_packed(&mut self, tick: u64, meta: u64, a: u64, b: u64) {
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
        // The bound the SAFETY argument below rests on, as a debug assertion: free in
        // a kernel build, and *live* in `verify/obs/fuzz.rs`, which is built with
        // debug assertions on precisely so a defect here names itself. Without it an
        // off-by-one in the mask above is a wild store and a segfault instead of a
        // failing check - the honest cost of a bounds-check-free store, paid here and
        // nowhere else in the tree.
        debug_assert!(
            slot < self.hdr.capacity as usize,
            "slot {slot} past capacity"
        );
        // SAFETY: `base_va` names `RING_EVENTS * 32` contiguous bytes this ring
        // adopted at fund time, non-zero exactly when `capacity` is - which the test
        // above established - and `slot < capacity == RING_EVENTS`, so all four
        // stores lie inside the block. Written as raw u64 stores to exactly the
        // `ObsEvent` layout (little-endian: `seq` is the packed word's low half),
        // asserted above against the real field offsets.
        unsafe {
            let p = (self.hdr.base_va as *mut u64).add(slot * WORDS);
            *p = tick;
            *p.add(1) = a;
            *p.add(2) = b;
            *p.add(3) = meta | seq_of(n) as u64;
        }
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
        let slot = (n & (cap - 1)) as usize;
        // SAFETY: the block is live (capacity nonzero above) and `slot < capacity`.
        // A volatile read, because on another CPU the writer may be storing into
        // this slot concurrently; the sequence check below is what decides whether
        // what was read is the generation asked for.
        let e =
            unsafe { core::ptr::read_volatile((self.hdr.base_va as *const ObsEvent).add(slot)) };
        if e.seq != seq_of(n) {
            // The writer recycled this slot between the bounds check and the read.
            return None;
        }
        Some(e)
    }

    /// Start the counter at an arbitrary point, so the host fuzzer can reach the
    /// wrap boundary.
    ///
    /// The boundary that matters is 2^32, where the recorded sequence number -
    /// `head`'s low 32 bits - recycles: about 71 minutes at one event per
    /// microsecond, so a boot never reaches it and would ship the arithmetic
    /// untested. Present for `verify/obs/fuzz.rs` and named so it is obvious no
    /// kernel path should call it - the same accommodation
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
/// "written first", which matters because a fresh block arrives zeroed.
#[inline]
pub const fn seq_of(n: u64) -> u32 {
    (n.wrapping_add(1) & 0xffff_ffff) as u32
}
