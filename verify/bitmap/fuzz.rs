// A model-checking fuzzer over the **shipped** free-frame bitmap search
// (kernel/src/mm/bitmap.rs, docs/SMP.md 10.0g), included verbatim.
//
// WHY A FUZZER AND NOT A BOOT TEST. The search replaced a bit-at-a-time loop with a
// word-at-a-time one, which is the same answer computed with four boundary
// conditions the old form did not have: the first word's low bits, the last word's
// high bits, both at once in a single-word range, and a range whose end is not a
// multiple of 64. Every one of those is a case where being wrong is **silent**. A
// missed free bit is a spurious out-of-memory on a machine with free memory; a bit
// returned from outside `[lo, hi)` is a frame on the wrong NUMA node, reported by
// `alloc_on` as correctly placed. Neither faults, and a boot exercises a handful of
// bitmap shapes out of 2^64.
//
// THE ORACLE IS INDEPENDENT, and deliberately stupid: a bit-at-a-time loop written
// here, not the shipped one refactored. It is the *pre-change* algorithm, so a
// disagreement means the optimisation changed an answer - which is the whole
// question. Computing the expected value from the code under test is the mistake
// verify/README.md records from the `entity` driver's first I5 check.
//
// WHAT IT CANNOT DO, said up front. This checks which bit the search returns. It
// does not check that the caller then sets that bit, charges `USED`, or moves the
// hint - those are `frames.rs`'s and are covered by the `numa`, `security` and `smp`
// kernels against frame-pool deltas. It also says nothing about locking: these are
// pure functions over a slice.
//
// Run it with `cargo xtask verify`.

// `bitmap.rs` is fully dependency-free - no statics, no `crate::` paths, plain
// functions over `&[u64]` - which is why it can be included with no shim at all.
// That was a design requirement of the module, not a happy accident.
#[path = "../../kernel/src/mm/bitmap.rs"]
mod bitmap;

/// The pre-change algorithm: the lowest clear bit in `[lo, hi)`, one bit at a time.
fn ref_find_in(words: &[u64], nbits: usize, lo: usize, hi: usize) -> Option<usize> {
    let hi = hi.min(nbits);
    for i in lo..hi {
        if words[i / 64] & (1u64 << (i % 64)) == 0 {
            return Some(i);
        }
    }
    None
}

/// The pre-change cyclic scan, written exactly as `frames::alloc` used to write it.
fn ref_find_from(words: &[u64], nbits: usize, from: usize) -> Option<usize> {
    if nbits == 0 {
        return None;
    }
    let from = if from >= nbits { 0 } else { from };
    for offset in 0..nbits {
        let i = (from + offset) % nbits;
        if words[i / 64] & (1u64 << (i % 64)) == 0 {
            return Some(i);
        }
    }
    None
}

/// The pre-change run scan, as `frames::alloc_contig` used to write it.
fn ref_find_run(words: &[u64], nbits: usize, n: usize) -> Option<usize> {
    if n == 0 || n > nbits {
        return None;
    }
    let mut run = 0usize;
    for i in 0..nbits {
        if words[i / 64] & (1u64 << (i % 64)) != 0 {
            run = 0;
            continue;
        }
        run += 1;
        if run == n {
            return Some(i + 1 - n);
        }
    }
    None
}

/// xorshift64*, so a failure is reproducible from its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// A bitmap of `nbits` bits with roughly `fill` of 256 bits set.
///
/// The density is varied rather than fixed because the two ends are different
/// failure modes: an almost-empty map returns on the first word and never exercises
/// the skip, while an almost-full one is the only shape that walks many words and
/// then has to get the *last* word's masking right.
fn make(rng: &mut Rng, nbits: usize, fill: u32) -> Vec<u64> {
    let nwords = nbits.div_ceil(64);
    let mut v = vec![0u64; nwords];
    for i in 0..nbits {
        if (rng.next() & 0xff) < u64::from(fill) {
            v[i / 64] |= 1u64 << (i % 64);
        }
    }
    // Bits at or above `nbits` in the final word are not frames. The shipped code
    // must never return one, and the reference never looks at them, so leaving them
    // **set** would let a bug hide and leaving them clear would let one be returned.
    // Clear is the honest choice: it is what the real allocator's zeroed static has,
    // and it means an off-by-one in the `hi` masking is *visible* as a returned bit
    // past the end rather than absorbed.
    for i in nbits..nwords * 64 {
        v[i / 64] &= !(1u64 << (i % 64));
    }
    v
}

fn main() {
    let mut failures: Vec<String> = Vec::new();
    let mut checks = 0u64;

    // ---- 1. hand-written boundary cases, before any randomness ----
    //
    // Each of these is a case the word-at-a-time form has and the bit-at-a-time form
    // did not, named so a failure says which boundary broke rather than "case 8231".
    let full = u64::MAX;
    let cases: &[(&str, Vec<u64>, usize, usize, usize, Option<usize>)] = &[
        ("empty map, whole range", vec![0, 0], 128, 0, 128, Some(0)),
        ("full map, whole range", vec![full, full], 128, 0, 128, None),
        ("lo mid-word", vec![0, 0], 128, 5, 128, Some(5)),
        ("lo mid-word, word full below", vec![0x1f, 0], 128, 5, 128, Some(5)),
        ("range inside one word", vec![!0x0f00u64, 0], 128, 8, 12, Some(8)),
        ("single-bit range, free", vec![0, 0], 128, 7, 8, Some(7)),
        ("single-bit range, taken", vec![1 << 7, 0], 128, 7, 8, None),
        ("empty range lo==hi", vec![0, 0], 128, 9, 9, None),
        ("inverted range", vec![0, 0], 128, 40, 9, None),
        // The one that matters most: `hi` on a word boundary makes the high mask
        // `!low_mask(64)`, i.e. the `b >= 64` arm that a naive `1 << b` would panic
        // on (or, in release, silently shift by 0 and mask off the whole word).
        ("hi exactly on word boundary", vec![full, 0], 128, 0, 64, None),
        ("hi on boundary, free below", vec![!(1u64 << 63), 0], 128, 0, 64, Some(63)),
        ("free only past hi", vec![full, 0], 128, 0, 64, None),
        ("second word only", vec![full, 0], 128, 64, 128, Some(64)),
        ("hi clamped to nbits", vec![full, !(1u64 << 5)], 70, 0, 999, Some(69)),
        ("nbits not a multiple of 64", vec![full, 0], 70, 0, 70, Some(64)),
        ("lo past nbits", vec![0, 0], 70, 90, 128, None),
    ];
    for (name, words, nbits, lo, hi, want) in cases {
        checks += 1;
        let got = bitmap::find_in(words, *nbits, *lo, *hi);
        if got != *want {
            failures.push(format!("find_in boundary '{name}': got {got:?}, want {want:?}"));
        }
        // And against the independent reference, which must agree with the
        // hand-computed answer too - if it does not, the hand answer is wrong.
        let r = ref_find_in(words, *nbits, *lo, *hi);
        if r != *want {
            failures.push(format!(
                "the REFERENCE disagrees with hand-computed '{name}': {r:?} vs {want:?} \
                 - the expected value in this table is wrong"
            ));
        }
    }

    // ---- 2. randomised equivalence, find_in ----
    let mut rng = Rng(0x5EED_1234_5EED_1234);
    for round in 0..40_000u32 {
        // Sizes that are and are not multiples of 64, small enough that ranges land
        // on interesting boundaries often.
        let nbits = 1 + rng.below(260);
        let fill = [0, 8, 64, 128, 200, 250, 256][rng.below(7)] as u32;
        let words = make(&mut rng, nbits, fill);
        for _ in 0..4 {
            let lo = rng.below(nbits + 8);
            let hi = rng.below(nbits + 8);
            checks += 1;
            let got = bitmap::find_in(&words, nbits, lo, hi);
            let want = ref_find_in(&words, nbits, lo, hi.min(nbits));
            if got != want {
                failures.push(format!(
                    "find_in round {round}: nbits={nbits} fill={fill} lo={lo} hi={hi} \
                     got {got:?} want {want:?}"
                ));
            }
            // The property that a wrong answer would violate even if it matched a
            // wrong reference: whatever comes back is inside the range and is free.
            if let Some(i) = got {
                if i < lo || i >= hi.min(nbits) {
                    failures.push(format!(
                        "find_in returned {i} outside [{lo}, {}) - on the NUMA path this \
                         is a frame on another node reported as placed",
                        hi.min(nbits)
                    ));
                }
                if words[i / 64] & (1u64 << (i % 64)) != 0 {
                    failures.push(format!("find_in returned {i}, which is allocated"));
                }
            }
        }
    }

    // ---- 3. randomised equivalence, find_from (the rotating hint) ----
    for round in 0..40_000u32 {
        let nbits = 1 + rng.below(260);
        let fill = [0, 32, 128, 240, 255, 256][rng.below(6)] as u32;
        let words = make(&mut rng, nbits, fill);
        for _ in 0..4 {
            let from = rng.below(nbits + 8);
            checks += 1;
            let got = bitmap::find_from(&words, nbits, from);
            let want = ref_find_from(&words, nbits, from);
            if got != want {
                failures.push(format!(
                    "find_from round {round}: nbits={nbits} fill={fill} from={from} \
                     got {got:?} want {want:?}"
                ));
            }
            if let Some(i) = got {
                if i >= nbits {
                    failures.push(format!("find_from returned {i} >= nbits {nbits}"));
                } else if words[i / 64] & (1u64 << (i % 64)) != 0 {
                    failures.push(format!("find_from returned {i}, which is allocated"));
                }
            }
        }
    }

    // ---- 4. randomised equivalence, find_run ----
    for round in 0..8_000u32 {
        let nbits = 1 + rng.below(260);
        let fill = [0, 32, 128, 200, 255][rng.below(5)] as u32;
        let words = make(&mut rng, nbits, fill);
        for _ in 0..4 {
            let n = rng.below(nbits + 4);
            checks += 1;
            let got = bitmap::find_run(&words, nbits, n);
            let want = ref_find_run(&words, nbits, n);
            if got != want {
                failures.push(format!(
                    "find_run round {round}: nbits={nbits} fill={fill} n={n} \
                     got {got:?} want {want:?}"
                ));
            }
            if let Some(start) = got {
                if start + n > nbits {
                    failures.push(format!("find_run {start}+{n} overruns nbits {nbits}"));
                } else if (start..start + n)
                    .any(|i| words[i / 64] & (1u64 << (i % 64)) != 0)
                {
                    failures.push(format!("find_run returned a run containing an allocated bit at {start}"));
                }
            }
        }
    }

    // ---- 5. the exhaustive small case ----
    //
    // Every 128-bit map is 2^128, but every map of **8** bits is 256, and every
    // (lo, hi) over 8 bits is 81 - so this corner is checked completely rather than
    // sampled, which is the only part of this driver that proves rather than tests.
    for pattern in 0u64..256 {
        let words = vec![pattern, 0];
        for nbits in 1..=8usize {
            for lo in 0..=8usize {
                for hi in 0..=8usize {
                    checks += 1;
                    if bitmap::find_in(&words, nbits, lo, hi)
                        != ref_find_in(&words, nbits, lo, hi.min(nbits))
                    {
                        failures.push(format!(
                            "exhaustive find_in: pattern={pattern:#04x} nbits={nbits} \
                             lo={lo} hi={hi}"
                        ));
                    }
                    checks += 1;
                    if bitmap::find_from(&words, nbits, lo) != ref_find_from(&words, nbits, lo) {
                        failures.push(format!(
                            "exhaustive find_from: pattern={pattern:#04x} nbits={nbits} \
                             from={lo}"
                        ));
                    }
                }
            }
        }
    }

    // ---- 6. the cost, reported ----
    //
    // Why this is measured here and not by `cargo xtask bench`: the icount benches
    // allocate from a nearly-empty pool, where the rotating hint points straight at a
    // free frame and BOTH algorithms terminate on the first candidate - so the suite
    // cannot see this change at all, and saying "the benches are unchanged" would be
    // true and beside the point. The win is on a **full region**, which is what
    // `alloc_on` faces once a NUMA node fills up, and it is a step count rather than
    // an instruction count.
    //
    // Both numbers are exact rather than sampled: a bit-at-a-time scan examines every
    // bit from `lo` up to and including the first free one, and a word-at-a-time scan
    // examines every word from `lo`'s up to and including that bit's. No instrumentation
    // needed, and nothing to get wrong by counting the wrong loop.
    //
    // The shape is the real one: the pool is 131,072 frames, and a node is half of it
    // on the two-node machine the `numa` kernel launches.
    {
        const NB: usize = 65_536;
        let mut bits_total = 0u64;
        let mut words_total = 0u64;
        let mut cases = 0u64;
        // A prefix of the region allocated, which is what a filling node looks like.
        for pct in [50u64, 75, 90, 99] {
            let taken = (NB as u64 * pct / 100) as usize;
            let mut words = vec![0u64; NB / 64];
            for i in 0..taken {
                words[i / 64] |= 1u64 << (i % 64);
            }
            let first = bitmap::find_in(&words, NB, 0, NB).expect("a free bit must remain");
            if first != taken {
                failures.push(format!(
                    "cost model: first free bit is {first}, expected {taken} - the model                      below would be measuring the wrong thing"
                ));
            }
            bits_total += (first + 1) as u64;
            words_total += (first / 64 + 1) as u64;
            cases += 1;
            println!(
                "  cost {pct:>2}% full: bit-at-a-time examines {} bits, word-at-a-time {} words",
                first + 1,
                first / 64 + 1
            );
        }
        if cases > 0 && words_total > 0 {
            println!(
                "  cost ratio over those shapes: {}x fewer steps",
                bits_total / words_total
            );
        }
    }

    println!("== bitmap search: shipped vs a bit-at-a-time reference ==");
    if failures.is_empty() {
        println!("  ok   {checks} checks, 0 disagreements");
        println!("       (16 hand-computed boundaries, 320,000 random find_in/find_from,");
        println!("        32,000 random find_run, and every 8-bit map exhaustively)");
        println!("bitmap fuzz: PASS");
    } else {
        for f in failures.iter().take(20) {
            println!("  FAIL {f}");
        }
        println!("  ... {} failure(s) total", failures.len());
        println!("bitmap fuzz: FAIL");
        std::process::exit(1);
    }
}
