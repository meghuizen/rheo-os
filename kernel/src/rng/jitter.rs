//! A **software-only entropy source**: CPU execution-time jitter
//! (docs/TIME-IDENTITY.md 4a).
//!
//! # Why a machine needs one
//!
//! Some machines have no randomness hardware at all - no RDSEED, no RNDR, no
//! virtio-rng, no TRNG chip. Without a source of their own they would be stuck
//! on the cycle-counter floor, which is not a source, it is a placeholder. This
//! is the fallback that makes such a machine seedable: the same idea as
//! `jitterentropy` and `haveged` on Linux, and the same idea as the interrupt
//! timings the pool already collects, but measured deliberately instead of
//! opportunistically.
//!
//! # Where the unpredictability comes from
//!
//! Run the *same* short piece of work twice on a real CPU and it takes a
//! different number of cycles. Caches, branch predictors, memory refresh,
//! bus arbitration, temperature and clock drift all move, and none of them is
//! visible to the code being timed. The measurement is the low bits of the
//! cycle-count difference across one round of work.
//!
//! The work itself is chosen so a compiler or an out-of-order CPU cannot
//! collapse it: a data-dependent walk over a scratch buffer, so each step's
//! address depends on the previous step's value, plus a mixing step whose
//! result feeds the next round.
//!
//! # Why this cannot fabricate entropy
//!
//! **This is the part that matters here, because under QEMU `-icount shift=0`
//! the cycle counter is deterministic** - the same work really does take the
//! same number of cycles, so there is genuinely no jitter to collect. A source
//! that credited entropy anyway would be a lie that every emulated boot tells.
//!
//! So the deltas are measured before they are counted:
//!
//! 1. **Repetition count** - no long run of identical deltas.
//! 2. **Adaptive proportion** - no single delta value dominating the window.
//! 3. **Distinct values** - the deltas must actually take many different
//!    values, which is the check that fails flat on an emulator.
//!
//! Failing any of them means **zero credit**. The samples are still mixed into
//! the pool (mixing can only help), they just cannot make the pool claim to be
//! seeded. A caller learns which happened from [`Report`].
//!
//! # How much is credited
//!
//! At most [`BITS_PER_SAMPLE`] bit per measurement, and only up to the number
//! of distinct values actually observed. One bit per timing sample is the
//! conservative figure jitterentropy uses after its own health tests; being
//! wrong in that direction costs a slower seed, being wrong the other way
//! costs the whole guarantee.

use crate::arch;

/// Measurements taken per gather. 256 samples at one credited bit each is a
/// full 256-bit seed from this source alone, if the health tests pass.
pub const SAMPLES: usize = 256;

/// Bits credited per measurement that survives the health tests.
pub const BITS_PER_SAMPLE: u32 = 1;

/// Scratch words the timed work walks. Big enough that the access pattern is
/// not trivially predictable, small enough to be a static (2 KiB).
const SCRATCH: usize = 256;

/// Fewest distinct delta values a window must contain before any of it counts.
/// A deterministic machine produces one or two; a real one produces dozens.
pub const MIN_DISTINCT_FOR_CREDIT: u32 = 16;

/// Longest run of identical deltas tolerated (SP 800-90B repetition count).
pub const MAX_RUN: u32 = 4;

/// What a [`gather`] found. Reported, never inferred - a caller prints this
/// rather than assuming which case it was in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Measurements taken.
    pub samples: u32,
    /// How many different delta values appeared.
    pub distinct: u32,
    /// Longest run of identical deltas.
    pub longest_run: u32,
    /// Bits credited - zero when the health tests failed.
    pub credited_bits: u32,
    /// Why nothing was credited, or `""` when it was.
    pub reason: &'static str,
}

/// The timed work's scratch. Static rather than a stack array so the buffer
/// address is the same each round, which keeps the *work* constant and leaves
/// only the machine's own variation in the measurement.
static mut SCRATCH_BUF: [u64; SCRATCH] = [0; SCRATCH];

/// One round of deliberately unpredictable work, returning a value the next
/// round depends on so the compiler cannot hoist or drop it.
#[inline(never)]
fn work(mut seed: u64) -> u64 {
    // SAFETY: a private static touched only from this function, which runs in
    // thread context on one CPU at a time (the pool lock is not held here, but
    // two cores gathering at once would only mix each other's work into their
    // own timing - which is more variation, never less, and the *values* are
    // never read as entropy, only the elapsed time is).
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH_BUF) };
    let mut i = (seed as usize) % SCRATCH;
    for _ in 0..64 {
        // Data-dependent address: the next index comes out of the word just
        // read, so the CPU cannot prefetch the chain.
        let v = buf[i] ^ seed;
        let mixed = v
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left((v & 63) as u32);
        buf[i] = mixed;
        seed = seed.wrapping_add(mixed) ^ (mixed >> 17);
        i = (mixed as usize) % SCRATCH;
    }
    seed
}

/// Take [`SAMPLES`] timing measurements, health-test them, and hand them to
/// the entropy pool with whatever credit they earned.
///
/// Returns what was measured. Thread context only.
pub fn gather() -> Report {
    let mut deltas = [0u32; SAMPLES];
    let mut seed = arch::cycles() ^ 0xA5A5_5A5A_DEAD_BEEF;
    for d in deltas.iter_mut() {
        let t0 = arch::cycles();
        seed = work(seed);
        let t1 = arch::cycles();
        // Low bits only: the high bits are the predictable bulk of the work,
        // the low bits are where the machine's own variation lives.
        *d = (t1.wrapping_sub(t0) & 0xFFFF) as u32;
    }

    let (distinct, longest_run) = describe(&deltas);

    // Mix regardless of the verdict - a suspect source can only help the pool,
    // it just must not be allowed to declare the pool ready.
    let mut bytes = [0u8; SAMPLES * 2];
    for (i, d) in deltas.iter().enumerate() {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&(*d as u16).to_le_bytes());
    }

    let (credited_bits, reason) = if longest_run > MAX_RUN {
        (0, "deltas repeat (no timing variation)")
    } else if distinct < MIN_DISTINCT_FOR_CREDIT {
        (0, "too few distinct deltas (emulated or fixed-cycle CPU)")
    } else if dominated(&deltas) {
        (0, "one delta value dominates the window")
    } else {
        // One bit per sample, but never more than the number of distinct
        // values seen - a window with 20 distinct values does not carry 256
        // bits however many samples it holds.
        let bits = (SAMPLES as u32 * BITS_PER_SAMPLE).min(distinct);
        (bits, "")
    };

    super::entropy::absorb(super::entropy::Source::Jitter, &bytes, credited_bits);

    Report {
        samples: SAMPLES as u32,
        distinct,
        longest_run,
        credited_bits,
        reason,
    }
}

/// Count distinct values and the longest run of identical ones.
fn describe(d: &[u32; SAMPLES]) -> (u32, u32) {
    let mut longest = 1u32;
    let mut run = 1u32;
    for i in 1..SAMPLES {
        if d[i] == d[i - 1] {
            run += 1;
            if run > longest {
                longest = run;
            }
        } else {
            run = 1;
        }
    }
    // Distinct count by scanning back over the prefix. O(n^2) at n = 256 is
    // ~32k comparisons, once per gather - cheaper than one page fault, and it
    // needs no allocation and no sort buffer.
    let mut distinct = 0u32;
    for i in 0..SAMPLES {
        let mut first = true;
        for j in 0..i {
            if d[j] == d[i] {
                first = false;
                break;
            }
        }
        if first {
            distinct += 1;
        }
    }
    (distinct, longest)
}

/// SP 800-90B adaptive proportion test: refuse a window where one value takes
/// more than half the samples, even if the rest look varied.
fn dominated(d: &[u32; SAMPLES]) -> bool {
    let cutoff = SAMPLES / 2;
    for i in 0..SAMPLES {
        let mut c = 0usize;
        for j in 0..SAMPLES {
            if d[j] == d[i] {
                c += 1;
            }
        }
        if c > cutoff {
            return true;
        }
        // Only the first few candidates need checking: a value taking more
        // than half the window must appear in the first half+1 positions.
        if i > cutoff {
            break;
        }
    }
    false
}
