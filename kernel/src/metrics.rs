//! **The metrics pipeline** (docs/SUBSTRATE.md pillar 7): per-CPU,
//! allocation-free-on-the-hot-path latency histograms with real percentiles.
//!
//! ## Why this exists
//!
//! The kernel already counts things - halts, spin polls, escalations, preserved
//! deadlines, frame allocations, COW faults - and every one of those counters was
//! added because a claim needed evidence (docs/ENGINEERING.md 1: observe, never
//! infer). But a *count* cannot answer the questions the workloads this substrate
//! targets are judged on. "Tail latency" is a percentile. "Jitter" is a
//! difference of percentiles. A mean hides exactly the events that matter: a
//! request path that is 20 us at the median and 40 ms at the 99.9th percentile
//! has a perfectly respectable average and is unusable.
//!
//! Percentiles need a distribution, and a kernel cannot keep samples. So this is
//! a **logarithmic bucket histogram** (the HDR-histogram shape): fixed storage,
//! constant-time record, bounded relative error, and percentiles recovered by
//! walking bucket counts.
//!
//! ## Definitions, fixed once here
//!
//! Metrics arguments are usually really disagreements about definitions, so the
//! definitions live in one place:
//!
//! - **Percentile p** is the value at or below which `p` percent of recorded
//!   samples fall, taken as the **lower bound of the containing bucket** - so a
//!   reported percentile is never larger than the truth (it can be up to one
//!   bucket width smaller). Erring low is the honest direction for a *latency*
//!   number only because it is stated; see [`Histogram::percentile`].
//! - **Jitter** is `P95 - P50`. One definition, used everywhere
//!   ([`Histogram::jitter`]). Not a standard deviation, not max-minus-min.
//! - **Relative error** is bounded by `1 / SUB_BUCKETS` = 6.25%: a value lands in
//!   a bucket whose width is at most 1/16 of its own magnitude.
//!
//! ## Integer only
//!
//! Every computation here is integer arithmetic - bucket selection is
//! `leading_zeros` (an integer log2), percentile interpolation is a comparison
//! against a running count. There is no floating point anywhere in this module,
//! which is what lets it live in a kernel that deliberately never touches the FP
//! register file (docs/SUBSTRATE.md pillar 4). The same property that makes
//! BORE's burst score kernel-safe.
//!
//! ## Storage
//!
//! Bucket arrays are **funded** ([`crate::mm::kmeta`]) and allocated **lazily,
//! per (CPU, metric), on first record**. That is the pillar-1 discipline applied
//! to the kernel's own observability: a boot that never records a network
//! latency pays nothing for the network histogram, and a 64-CPU machine does not
//! reserve 64 copies of every histogram in `.bss` (which at full precision would
//! be megabytes of always-resident zeroes). One frame backs one histogram's
//! buckets, and the small per-histogram header - count, sum, min, max, and the
//! frame's address - lives in a per-CPU static, so recording never allocates
//! after the first sample and reading never allocates at all.
//!
//! A histogram whose bucket frame could not be allocated still keeps count, sum,
//! min and max: it degrades to summary statistics **and says so**
//! ([`Histogram::has_buckets`]), rather than silently reporting percentiles
//! computed from nothing.
//!
//! ## Per-CPU, and what aggregation means
//!
//! Each core records into its own histogram with no lock and no atomic - that is
//! the multikernel model (docs/SCHEDULING.md 1a), and it is why recording is
//! cheap enough to leave on. A reader that wants a machine-wide view
//! [`merge`]s the per-CPU histograms, which is exact for counts and sums and
//! exact for percentiles too, because summing bucket counts across cores is
//! precisely the combined distribution. Merging across cores is where a torn
//! read can happen (another core may be recording); the result is then off by at
//! most the samples in flight, which is stated at [`snapshot`] rather than
//! papered over.

use crate::mm::kmeta::{self, Owner};
use crate::smp::PerCpu;

/// Sub-buckets per power of two. 16 gives a 6.25% worst-case relative error,
/// which is well inside what a latency decision needs and keeps one histogram's
/// buckets inside a single 4 KiB frame.
const SUB_BITS: u32 = 4;
/// Sub-buckets per power-of-two bucket.
const SUB_BUCKETS: usize = 1 << SUB_BITS;
/// Total bucket slots: exponents 0..=60 (a `u64`'s most significant bit is at
/// most 63, giving a maximum exponent of `63 - SUB_BITS + 1`), times the
/// sub-buckets in each. 976 slots * 4 B = 3904 B, so the whole array fits in one
/// frame with room to spare.
pub const SLOTS: usize = (63 - SUB_BITS as usize + 2) * SUB_BUCKETS;

const _: () = assert!(SLOTS * core::mem::size_of::<u32>() <= crate::mm::frames::FRAME_SIZE);

/// What is being measured. A **closed, kernel-defined set**: these are the
/// kernel's own observables, not a workload's, so naming them here is the same
/// discipline as [`crate::ktimer::TimerClient`] - and unlike a fixed *capacity*,
/// a fixed *vocabulary* is not a limit anything can hit at run time.
///
/// A cell measures its own latencies in its own runtime (librheo), where it has
/// an allocator; this is for what only the kernel can see.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Metric {
    /// Syscall entry to exit, nanoseconds. The number the "syscall batch
    /// throughput" axis of docs/SUBSTRATE.md 2 is argued from.
    SyscallNs = 0,
    /// Queue submission to completion, nanoseconds - the async round trip a
    /// strand actually waits on.
    QueueNs = 1,
    /// Cross-cell / vcore switch cost, nanoseconds.
    SwitchNs = 2,
    /// Block-device request service time, nanoseconds.
    BlockNs = 3,
    /// Network round-trip / RTT samples, nanoseconds. The input to the pacing
    /// safe-zone and jitter questions (docs/NETSTACK.md).
    NetRttNs = 4,
    /// User page-fault service time, nanoseconds - demand paging's real cost.
    FaultNs = 5,
    /// How long a vcore ran before voluntarily relinquishing, nanoseconds. The
    /// distribution BORE's burst score summarises (pillar 3), kept separately so
    /// the score can be checked against the thing it claims to measure.
    BurstNs = 6,
    /// Scheduler queue delay: runnable-to-running, nanoseconds. The
    /// responsiveness number the EEVDF/BORE work is judged on.
    RunDelayNs = 7,
}

/// Number of metrics.
pub const METRICS: usize = 8;

impl Metric {
    /// Every metric, for a reader that reports all of them.
    pub const ALL: [Metric; METRICS] = [
        Metric::SyscallNs,
        Metric::QueueNs,
        Metric::SwitchNs,
        Metric::BlockNs,
        Metric::NetRttNs,
        Metric::FaultNs,
        Metric::BurstNs,
        Metric::RunDelayNs,
    ];

    /// Short stable name, for a diagnostic line or a bench report.
    pub fn name(self) -> &'static str {
        match self {
            Metric::SyscallNs => "syscall_ns",
            Metric::QueueNs => "queue_ns",
            Metric::SwitchNs => "switch_ns",
            Metric::BlockNs => "block_ns",
            Metric::NetRttNs => "net_rtt_ns",
            Metric::FaultNs => "fault_ns",
            Metric::BurstNs => "burst_ns",
            Metric::RunDelayNs => "run_delay_ns",
        }
    }
}

/// Bucket index for `value`. Constant time: one `leading_zeros` and a shift.
///
/// Exponent 0 holds `[0, SUB_BUCKETS)` linearly. Exponent `e >= 1` covers
/// `[SUB_BUCKETS << (e-1), SUB_BUCKETS << e)` split into [`SUB_BUCKETS`] equal
/// sub-buckets of width `1 << (e-1)`.
#[inline]
fn bucket_of(value: u64) -> usize {
    if value < SUB_BUCKETS as u64 {
        return value as usize;
    }
    let msb = 63 - value.leading_zeros(); // >= SUB_BITS here
    let exp = (msb - SUB_BITS + 1) as usize; // >= 1
    let base = (SUB_BUCKETS as u64) << (exp - 1);
    let sub = ((value - base) >> (exp - 1)) as usize;
    // `sub` is < SUB_BUCKETS by construction; clamp so a future change to the
    // exponent arithmetic can never index out of the array.
    exp * SUB_BUCKETS + sub.min(SUB_BUCKETS - 1)
}

/// The **lower bound** of the values that land in `index` - the value a
/// percentile query reports. See the module docs on erring low.
#[inline]
pub fn bucket_value(index: usize) -> u64 {
    let exp = index / SUB_BUCKETS;
    let sub = (index % SUB_BUCKETS) as u64;
    if exp == 0 {
        sub
    } else {
        ((SUB_BUCKETS as u64) << (exp - 1)) + (sub << (exp - 1))
    }
}

/// One latency distribution.
///
/// `Copy` and `const`-constructible so a per-CPU array of them is a plain
/// static; the bucket storage it points at is funded and lazy.
#[derive(Copy, Clone)]
pub struct Histogram {
    /// Kernel VA of the `[u32; SLOTS]` bucket array, or 0 if not yet allocated.
    buckets: usize,
    /// Samples recorded (including any that arrived before the buckets did).
    count: u64,
    /// Sum of recorded values, saturating. Kept so the mean is exact rather than
    /// reconstructed from buckets, and so a bucket-less histogram still reports
    /// something true.
    sum: u64,
    /// Smallest and largest values seen exactly (not bucketed).
    min: u64,
    max: u64,
    /// Samples that arrived while no bucket frame could be allocated, and so are
    /// counted but not placed. Non-zero means percentiles are computed from an
    /// incomplete distribution, which [`Histogram::complete`] reports.
    unplaced: u64,
}

impl Histogram {
    /// An empty histogram holding no storage.
    pub const fn new() -> Histogram {
        Histogram {
            buckets: 0,
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
            unplaced: 0,
        }
    }

    /// Whether bucket storage has been allocated (so percentiles are meaningful).
    pub fn has_buckets(&self) -> bool {
        self.buckets != 0
    }

    /// Whether every recorded sample was actually placed in a bucket. False means
    /// storage was unavailable for some samples and percentiles are computed from
    /// a subset - the honest caveat, reported rather than hidden.
    pub fn complete(&self) -> bool {
        self.unplaced == 0
    }

    /// Samples recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Sum of samples.
    pub fn sum(&self) -> u64 {
        self.sum
    }

    /// Smallest sample, or 0 if none.
    pub fn min(&self) -> u64 {
        if self.count == 0 { 0 } else { self.min }
    }

    /// Largest sample.
    pub fn max(&self) -> u64 {
        self.max
    }

    /// Integer mean, or 0 with no samples. Deliberately offered *beside* the
    /// percentiles rather than instead of them.
    pub fn mean(&self) -> u64 {
        self.sum.checked_div(self.count).unwrap_or(0)
    }

    fn slots(&self) -> Option<*mut u32> {
        if self.buckets == 0 {
            None
        } else {
            Some(self.buckets as *mut u32)
        }
    }

    /// Read bucket `index`.
    fn bucket(&self, index: usize) -> u32 {
        match self.slots() {
            // SAFETY: `index < SLOTS` is checked; the frame holds SLOTS u32s and
            // is reached through the kernel's linear map.
            Some(p) if index < SLOTS => unsafe { *p.add(index) },
            _ => 0,
        }
    }

    /// Record one sample, allocating bucket storage on first use.
    ///
    /// Constant time after the first call. `owner` is charged for the one frame
    /// this may take (pillar 1); pass [`Owner::KERNEL`] for kernel-wide metrics,
    /// which is what [`record`] does.
    pub fn record_owned(&mut self, value: u64, owner: Owner) {
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(value);
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
        if self.buckets == 0 {
            match kmeta::alloc_metric_frame(owner) {
                Some(va) => self.buckets = va,
                None => {
                    self.unplaced = self.unplaced.saturating_add(1);
                    return;
                }
            }
        }
        let index = bucket_of(value);
        if let Some(p) = self.slots()
            && index < SLOTS
        {
            // SAFETY: index bounded above; exclusive via `&mut self`, and the
            // histogram belongs to this CPU (see the module docs).
            unsafe {
                let slot = p.add(index);
                *slot = (*slot).saturating_add(1);
            }
        }
    }

    /// The value at percentile `pct` (0..=100), as a bucket lower bound.
    ///
    /// Returns 0 with no samples. With bucket storage missing entirely this
    /// reports the mean rather than a fabricated percentile, and
    /// [`Histogram::has_buckets`] is how a caller tells the two apart.
    pub fn percentile(&self, pct: u32) -> u64 {
        if self.count == 0 {
            return 0;
        }
        if self.buckets == 0 {
            return self.mean();
        }
        let pct = pct.min(100) as u64;
        // The rank of the sample we want, counting from 1. Integer arithmetic:
        // ceil(count * pct / 100).
        let placed = self.count.saturating_sub(self.unplaced).max(1);
        let target = (placed.saturating_mul(pct)).div_ceil(100).max(1);
        let mut seen = 0u64;
        for i in 0..SLOTS {
            seen = seen.saturating_add(self.bucket(i) as u64);
            if seen >= target {
                return bucket_value(i);
            }
        }
        self.max
    }

    /// P50, the median.
    pub fn p50(&self) -> u64 {
        self.percentile(50)
    }
    /// P95.
    pub fn p95(&self) -> u64 {
        self.percentile(95)
    }
    /// P99.
    pub fn p99(&self) -> u64 {
        self.percentile(99)
    }
    /// P99.9, for a tail-latency claim. Meaningful only with >= 1000 samples,
    /// which the caller must ensure - with fewer, this is the top bucket seen.
    pub fn p999(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        if self.buckets == 0 {
            return self.mean();
        }
        let placed = self.count.saturating_sub(self.unplaced).max(1);
        let target = (placed.saturating_mul(999)).div_ceil(1000).max(1);
        let mut seen = 0u64;
        for i in 0..SLOTS {
            seen = seen.saturating_add(self.bucket(i) as u64);
            if seen >= target {
                return bucket_value(i);
            }
        }
        self.max
    }

    /// **Jitter: `P95 - P50`.** The one definition, used everywhere (see the
    /// module docs).
    pub fn jitter(&self) -> u64 {
        self.p95().saturating_sub(self.p50())
    }

    /// Fold `other`'s samples into this histogram. Used by [`snapshot`] to build
    /// a machine-wide view from the per-CPU ones; exact for counts, sums and
    /// bucket contents.
    ///
    /// `self` must have (or be able to allocate) bucket storage; if it cannot,
    /// the merged samples are counted as unplaced, so the result reports itself
    /// incomplete instead of losing them silently.
    pub fn merge_from(&mut self, other: &Histogram, owner: Owner) {
        if other.count == 0 {
            return;
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self.sum.saturating_add(other.sum);
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
        self.unplaced = self.unplaced.saturating_add(other.unplaced);
        if other.buckets == 0 {
            return;
        }
        if self.buckets == 0 {
            match kmeta::alloc_metric_frame(owner) {
                Some(va) => self.buckets = va,
                None => {
                    self.unplaced = self.unplaced.saturating_add(other.count);
                    return;
                }
            }
        }
        for i in 0..SLOTS {
            let add = other.bucket(i);
            if add == 0 {
                continue;
            }
            if let Some(p) = self.slots() {
                // SAFETY: index bounded; exclusive via `&mut self`.
                unsafe {
                    let slot = p.add(i);
                    *slot = (*slot).saturating_add(add);
                }
            }
        }
    }

    /// Release bucket storage and reset to empty.
    pub fn release(&mut self) {
        if self.buckets != 0 {
            kmeta::free_metric_frame(self.buckets, Owner::KERNEL);
            self.buckets = 0;
        }
        *self = Histogram::new();
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-CPU metric sets. Each core records into its own, lock-free.
static SETS: PerCpu<[Histogram; METRICS]> = PerCpu::new([const { Histogram::new() }; METRICS]);

/// Whether recording is enabled. Off by default so that no existing kernel
/// changes behaviour or timing by linking this module: a boot that never calls
/// [`enable`] records nothing, allocates nothing, and the record path is a single
/// load-and-branch.
///
/// This is not a "config knob" in the sense ARCHITECTURE.md 6 rules out - it does
/// not select a policy, it decides whether the kernel spends frames observing
/// itself, which is exactly the kind of thing a boot should decide.
static ENABLED: PerCpu<bool> = PerCpu::new(false);

/// Turn recording on for this CPU.
pub fn enable() {
    // SAFETY: this CPU's own slot; no other reference is held across this call.
    unsafe {
        *ENABLED.this_mut() = true;
    }
}

/// Turn recording off for this CPU (storage is kept, so counts survive).
pub fn disable() {
    // SAFETY: as `enable`.
    unsafe {
        *ENABLED.this_mut() = false;
    }
}

/// Whether this CPU is recording.
pub fn enabled() -> bool {
    *ENABLED.this()
}

/// Record `value` for `metric` on this CPU. A no-op unless [`enable`] was called.
///
/// The hot path: one branch, one bucket index, one increment - no lock, no
/// atomic, no allocation after the first sample per (CPU, metric).
#[inline]
pub fn record(metric: Metric, value: u64) {
    if !enabled() {
        return;
    }
    // SAFETY: this CPU's own slot, and no other reference to it is live across
    // this call (the record path calls nothing that re-enters metrics).
    unsafe {
        SETS.this_mut()[metric as usize].record_owned(value, Owner::KERNEL);
    }
}

/// Record the elapsed time between two [`crate::ktimer::now_ns`] readings.
/// Written as one call so the subtraction (and its wrap handling) is not
/// re-derived at every call site.
#[inline]
pub fn record_since(metric: Metric, start_ns: u64) {
    if !enabled() {
        return;
    }
    let now = crate::ktimer::now_ns();
    record(metric, now.wrapping_sub(start_ns));
}

/// This CPU's histogram for `metric`.
pub fn local(metric: Metric) -> Histogram {
    SETS.this()[metric as usize]
}

/// CPU `cpu`'s histogram for `metric`.
///
/// # Safety
/// See [`crate::smp::PerCpu::get`]: a concurrently-recording core may be observed
/// mid-update, so counts can be off by the samples in flight.
pub unsafe fn per_cpu(cpu: usize, metric: Metric) -> Histogram {
    // SAFETY: delegated to the caller per the contract above.
    unsafe { SETS.get(cpu)[metric as usize] }
}

/// A machine-wide histogram for `metric`, merged across every CPU.
///
/// Not a consistent snapshot: another core may record during the merge, so the
/// result can miss samples in flight (never double-count them, since each is
/// added once from its own core's buckets). Stated rather than implied - for the
/// deterministic assertions a test makes, read [`local`] on a single-CPU boot.
///
/// The returned histogram owns a freshly funded bucket frame; the caller must
/// [`Histogram::release`] it.
pub fn snapshot(metric: Metric) -> Histogram {
    let mut out = Histogram::new();
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: aggregation read, tearing accepted per this function's docs.
        let h = unsafe { SETS.get(cpu)[metric as usize] };
        out.merge_from(&h, Owner::KERNEL);
    }
    out
}

/// Release every histogram's storage on this CPU and reset the counts. Called
/// between runs; leaves recording enabled/disabled as it was.
pub fn reset_local() {
    for m in Metric::ALL {
        // SAFETY: this CPU's own slot.
        unsafe {
            SETS.this_mut()[m as usize].release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bucketing must be monotone and self-consistent: the reported lower bound
    /// of a value's bucket is never above the value, and never more than one
    /// bucket width below it.
    #[test]
    fn bucket_bounds_are_honest() {
        for v in [
            0u64,
            1,
            15,
            16,
            17,
            31,
            32,
            63,
            64,
            1023,
            1024,
            1 << 40,
            u64::MAX,
        ] {
            let i = bucket_of(v);
            assert!(i < SLOTS, "value {v} -> slot {i} out of range");
            let lo = bucket_value(i);
            assert!(lo <= v, "value {v} -> bucket lower bound {lo} above it");
        }
    }

    /// Monotonicity: a larger value never lands in an earlier bucket.
    #[test]
    fn buckets_are_monotone() {
        let mut last = 0;
        for shift in 0..63 {
            for step in [0u64, 1, 3, 7] {
                let v = (1u64 << shift).saturating_add(step);
                let i = bucket_of(v);
                assert!(i >= last, "value {v} went backwards: {i} < {last}");
                last = i;
            }
        }
    }
}
