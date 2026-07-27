//! **BORE: burst-oriented response enhancement** (docs/SCHEDULING.md 11.4,
//! docs/SUBSTRATE.md pillar 3).
//!
//! ## What a burst score is, and why this OS can measure it honestly
//!
//! A scheduler that wants to be responsive has to distinguish work that is
//! *waiting on the world* from work that is *using the CPU*. Priorities do not
//! tell it that (a task declares them, and a task that lies is rewarded), so
//! modern Linux infers it from behaviour: BORE tracks each task's **burst time** -
//! the CPU time it consumed since it last **voluntarily relinquished** the CPU -
//! and weights short-burst tasks up and long-burst tasks down. The equilibrium
//! favours interactive work without anyone declaring anything.
//!
//! On Linux that inference is genuinely an inference: "went to sleep" has to be
//! recognised inside a kernel that also owns the I/O, and a task blocking on a
//! page fault, a lock, or a syscall are different code paths that must all be
//! caught. Here it is an **observation**. Every relinquish in this OS is an
//! explicit, already-instrumented syscall-boundary transition - a strand parking
//! on a queue completion, `SYS_WAIT_*`, `SYS_YIELD`, a channel receive, the
//! per-context block the Linux personality records in `thread.rs` `pblock`. The
//! burst score is computed from marks the kernel already makes, which is the
//! difference between docs/ENGINEERING.md 1's "observe" and its "infer".
//!
//! ## The arithmetic is integer, and that is load-bearing
//!
//! The score is the **bit length** of the normalised burst time - an integer
//! log2, one `leading_zeros` - scaled by a fixed-point factor. No division by a
//! running average, no `vruntime`-style rational arithmetic, and above all no
//! floating point: this runs in a kernel that never touches the FP register file
//! (docs/SUBSTRATE.md pillar 4), so a scheduler heuristic that wanted a float
//! would either force FP into kernel context or be approximated badly. BORE
//! wanting a bit-length is what makes the most current responsiveness heuristic
//! in production Linux directly usable here.
//!
//! Suzuki's framing is a radix conversion: a binary logarithm turned into a
//! common-logarithm-shaped weight, mapping a burst range spanning nanoseconds to
//! minutes onto a roughly 0.01x-100x weight range, dimensionlessly.
//!
//! ## Constants
//!
//! Taken from the BORE scheduler's own defaults so the behaviour is comparable to
//! the production implementation rather than invented here:
//!
//! - [`PENALTY_OFFSET_BITS`] = 24: bits subtracted before taking the bit length,
//!   so bursts shorter than ~16.8 ms score zero and are not penalised at all.
//!   This is what keeps ordinary interactive work at full weight.
//! - [`PENALTY_SCALE`] = 1536 in 1/1024 units (1.5x): how sharply the bit length
//!   turns into score steps.
//! - [`SCORE_MAX`] = 39: the score range is `0..=39`, deliberately the same span
//!   as `nice`, so one step is ~1.25x of weight - the ratio Linux's own weight
//!   table uses, which is what makes the [`weight_of`] table's shape familiar and
//!   its total range ~10^4.
//!
//! ## Smoothing and inheritance
//!
//! Two refinements from BORE that matter, both about not letting one measurement
//! decide too much:
//!
//! - **The score is smoothed against history** ([`Burst::score`] takes the larger
//!   of the latest and the remembered score, decaying the memory), so a single
//!   long burst does not permanently demote a task and a single short one does
//!   not instantly promote a batch job.
//! - **A child inherits its parent's burst** ([`Burst::inherit`]). Without this a
//!   build that forks CPU-hungry children lets each child start at "perfectly
//!   interactive" and swamp real interactive work - the classic `make -j`
//!   pathology. Here the cell tree is the natural carrier: a cell created by
//!   `fork`/`SYS_SPAWN` starts from its parent's observed burst rather than from
//!   a neutral default.

/// Bits subtracted from the burst time before taking its bit length. Bursts below
/// `1 << 24` ns (~16.8 ms) therefore score 0.
pub const PENALTY_OFFSET_BITS: u32 = 24;

/// Score scaling, in 1/1024 units. 1536 = 1.5x.
pub const PENALTY_SCALE: u64 = 1536;

/// Highest burst score. The range `0..=SCORE_MAX` mirrors `nice`'s span.
pub const SCORE_MAX: u8 = 39;

/// Weight of a score-0 (fully interactive) vcore. Chosen so that the 40-step
/// 1.25x-per-step ladder bottoms out near 1, giving the ~10^4 total range BORE
/// describes without any entry underflowing to zero.
pub const WEIGHT_BASE: u64 = 10240;

/// Weight ladder: `WEIGHT[s]` is the scheduling weight of burst score `s`.
///
/// Built at compile time as `WEIGHT_BASE * 0.8^s` (each score step is 1/1.25 of
/// the weight below it), floored at 1 so a maximally greedy vcore is still
/// schedulable - starvation is not a policy this scheduler is allowed to have
/// (docs/SCHEDULING.md: importance is a contract, never a priority, and residual
/// work still runs on slack).
const WEIGHT: [u64; SCORE_MAX as usize + 1] = build_weights();

const fn build_weights() -> [u64; SCORE_MAX as usize + 1] {
    let mut table = [0u64; SCORE_MAX as usize + 1];
    let mut w = WEIGHT_BASE;
    let mut i = 0;
    while i < table.len() {
        table[i] = if w == 0 { 1 } else { w };
        // Integer 1/1.25 = *4/5, computed before the store so index 0 keeps the
        // base weight exactly.
        w = w * 4 / 5;
        i += 1;
    }
    table
}

/// The scheduling weight for a burst score. Higher score (greedier) = lower
/// weight = later virtual deadline = served after more modest work.
#[inline]
pub fn weight_of(score: u8) -> u64 {
    WEIGHT[(score.min(SCORE_MAX)) as usize]
}

/// Turn an accumulated burst time into a score.
///
/// `bitlen(burst_ns >> PENALTY_OFFSET_BITS) * PENALTY_SCALE / 1024`, clamped to
/// [`SCORE_MAX`]. One `leading_zeros`, one multiply, one shift - no division, no
/// float, no loop.
#[inline]
pub fn score_of(burst_ns: u64) -> u8 {
    let shifted = burst_ns >> PENALTY_OFFSET_BITS;
    if shifted == 0 {
        return 0;
    }
    // Bit length: the position of the highest set bit, plus one.
    let bitlen = (64 - shifted.leading_zeros()) as u64;
    let scaled = (bitlen * PENALTY_SCALE) >> 10;
    if scaled > SCORE_MAX as u64 {
        SCORE_MAX
    } else {
        scaled as u8
    }
}

/// One vcore's burst state: what it has consumed since it last yielded, and the
/// smoothed score that follows from it.
#[derive(Copy, Clone, Debug)]
pub struct Burst {
    /// CPU nanoseconds consumed since the last voluntary relinquish.
    accumulated_ns: u64,
    /// Smoothed score carried across bursts (the history term).
    history: u8,
    /// Voluntary relinquishes observed - the denominator of "how often does this
    /// thing actually wait", and the evidence that the score came from measured
    /// behaviour rather than a default.
    yields: u64,
    /// Longest single burst seen, for a diagnostic and for the test oracle.
    peak_ns: u64,
}

impl Burst {
    /// A fresh burst state: no history, treated as fully interactive until it
    /// demonstrates otherwise.
    ///
    /// Optimistic on purpose. A new vcore is usually a new request, a new
    /// connection, or a shell command - the things a responsive system should
    /// serve first - and if it turns out to be a compute job its first burst
    /// demotes it within one slice. The opposite default (assume greedy) would
    /// penalise exactly the interactive work this exists to protect.
    pub const fn new() -> Burst {
        Burst {
            accumulated_ns: 0,
            history: 0,
            yields: 0,
            peak_ns: 0,
        }
    }

    /// A child's initial state, inherited from its parent (see the module docs on
    /// the `make -j` pathology). The child takes the parent's *history*, not its
    /// in-flight accumulation, because the child has not run yet.
    pub const fn inherit(parent: &Burst) -> Burst {
        Burst {
            accumulated_ns: 0,
            history: parent.history,
            yields: 0,
            peak_ns: 0,
        }
    }

    /// Charge `delta_ns` of CPU time to the current burst.
    #[inline]
    pub fn charge(&mut self, delta_ns: u64) {
        self.accumulated_ns = self.accumulated_ns.saturating_add(delta_ns);
        if self.accumulated_ns > self.peak_ns {
            self.peak_ns = self.accumulated_ns;
        }
    }

    /// Record a **voluntary relinquish**: the burst ends, its score is folded into
    /// the history, and accumulation restarts.
    ///
    /// This is the only place a burst ends. A vcore *preempted* by the timer has
    /// not relinquished and keeps accumulating, which is the entire point - being
    /// forced off the CPU is evidence of greed, not of politeness.
    pub fn relinquish(&mut self) {
        let latest = score_of(self.accumulated_ns);
        // BORE's smoothing: take the larger of the latest and the decayed history,
        // so a spike is remembered for a while but does not stick forever.
        let decayed = self.history.saturating_sub(1);
        self.history = if latest > decayed { latest } else { decayed };
        self.accumulated_ns = 0;
        self.yields = self.yields.saturating_add(1);
    }

    /// Record being **preempted** (forced off the CPU). Keeps the burst running:
    /// see [`Burst::relinquish`].
    pub fn preempted(&mut self) {
        // Nothing to do - the accumulation continues. Present as a named call so
        // the distinction between the two ways of losing the CPU is explicit at
        // every call site rather than being "whichever function we remembered".
    }

    /// The current score: the larger of the in-flight burst's score and the
    /// smoothed history, so a vcore that is *currently* running long is demoted
    /// immediately rather than only after it finally yields.
    #[inline]
    pub fn score(&self) -> u8 {
        let live = score_of(self.accumulated_ns);
        if live > self.history {
            live
        } else {
            self.history
        }
    }

    /// This vcore's scheduling weight.
    #[inline]
    pub fn weight(&self) -> u64 {
        weight_of(self.score())
    }

    /// Nanoseconds in the current (unfinished) burst.
    pub fn accumulated_ns(&self) -> u64 {
        self.accumulated_ns
    }

    /// Voluntary relinquishes observed.
    pub fn yields(&self) -> u64 {
        self.yields
    }

    /// Longest single burst seen.
    pub fn peak_ns(&self) -> u64 {
        self.peak_ns
    }

    /// The smoothed history score, without the in-flight term.
    pub fn history(&self) -> u8 {
        self.history
    }
}

impl Default for Burst {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset must genuinely exempt short bursts: an interactive handler that
    /// runs for a millisecond and yields must score zero, or the whole scheme
    /// penalises the work it exists to protect.
    #[test]
    fn short_bursts_score_zero() {
        for ns in [0u64, 1_000, 100_000, 1_000_000, 16_000_000] {
            assert_eq!(score_of(ns), 0, "burst {ns} ns should not be penalised");
        }
    }

    /// A long burst must score above a short one, and the mapping must be
    /// monotone - the property the weight ladder depends on.
    #[test]
    fn score_is_monotone_in_burst() {
        let mut last = 0;
        let mut ns = 1u64 << PENALTY_OFFSET_BITS;
        while ns < (1u64 << 62) {
            let s = score_of(ns);
            assert!(s >= last, "score went backwards at {ns} ns");
            last = s;
            ns <<= 1;
        }
        assert!(last > 0, "no burst length ever scored above zero");
    }

    /// Weights must be strictly non-increasing in score, span about four orders
    /// of magnitude, and never reach zero (which would make a vcore unschedulable
    /// rather than merely low-priority).
    #[test]
    fn weight_ladder_is_sane() {
        for s in 1..=SCORE_MAX {
            assert!(
                weight_of(s) <= weight_of(s - 1),
                "weight rose from score {} to {s}",
                s - 1
            );
            assert!(weight_of(s) >= 1, "score {s} has zero weight");
        }
        assert_eq!(weight_of(0), WEIGHT_BASE);
        let ratio = weight_of(0) / weight_of(SCORE_MAX);
        assert!(ratio >= 1000, "weight range {ratio}x is too narrow");
    }

    /// A preempted vcore keeps accumulating; only a voluntary relinquish ends the
    /// burst. This is the distinction the whole heuristic rests on.
    #[test]
    fn preemption_does_not_end_a_burst() {
        let mut b = Burst::new();
        b.charge(1 << 30); // ~1 s
        b.preempted();
        assert_eq!(b.accumulated_ns(), 1 << 30);
        assert!(b.score() > 0, "a long run should be demoted while running");
        b.relinquish();
        assert_eq!(b.accumulated_ns(), 0);
        assert_eq!(b.yields(), 1);
        assert!(b.history() > 0, "the burst should be remembered");
    }

    /// A child must not start at "fully interactive" when its parent is a compute
    /// hog, or a forking build swamps interactive work.
    #[test]
    fn children_inherit_burst_history() {
        let mut parent = Burst::new();
        parent.charge(1 << 34);
        parent.relinquish();
        let child = Burst::inherit(&parent);
        assert_eq!(child.history(), parent.history());
        assert!(child.weight() < weight_of(0));
    }
}
