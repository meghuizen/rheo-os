//! Free-frame search over the allocator's bitmap, **a word at a time**.
//!
//! # Why this is its own module
//!
//! The allocator's search was a bit-by-bit loop: for each candidate frame, index a
//! word, shift, mask, test. That is O(frames examined) with a per-*frame* constant,
//! and the frames it examines are the **allocated** ones - so the cost of finding a
//! free frame grows with how full the region is. On the unrestricted path the
//! rotating hint usually points straight at a free frame and it never showed, but
//! [`find_in`]'s caller (`alloc_on`, the NUMA path) restarts at the node's `lo`
//! **every call**, so a node that is 90% full paid ~59,000 iterations per
//! allocation, and running one dry - which the `numa` kernel does deliberately -
//! is quadratic in the node's size.
//!
//! A word at a time turns each 64 allocated frames into one load, one compare and
//! one branch, and finds the free bit with a single `trailing_zeros`. Same answer,
//! ~64x fewer steps through a full region.
//!
//! It is a **separate, dependency-free module** rather than inline in `frames.rs`
//! for one reason: this is bit arithmetic with four boundary conditions (the first
//! word's low bits, the last word's high bits, both at once in a single-word range,
//! and a range whose end is not a multiple of 64), and every one of them is a case
//! where getting it wrong is silent. A missed free bit is a spurious out-of-memory;
//! a bit returned from outside `[lo, hi)` is a frame on the **wrong NUMA node**,
//! reported as correctly placed. Neither faults. So the functions take a plain
//! `&[u64]` and no kernel state, which lets `verify/bitmap/` drive them on the host
//! against a naive bit-by-bit reference over random bitmaps and random ranges -
//! millions of cases including every boundary, none of which a boot reaches.
//!
//! Convention throughout: a **set** bit means allocated, a **clear** bit means free,
//! and bit `i` of the bitmap is frame `i` - word `i / 64`, bit `i % 64`.

/// Bits `[0, b)` set, for `b` in `0..=64`.
///
/// `1u64 << 64` is undefined-by-panic in Rust, and `b == 64` is reachable from
/// [`find_in`] whenever a range ends exactly on a word boundary, so the wide case is
/// answered directly rather than by shifting.
#[inline]
const fn low_mask(b: usize) -> u64 {
    if b >= 64 { u64::MAX } else { (1u64 << b) - 1 }
}

/// The lowest free bit in `[lo, hi)`, or `None`.
///
/// `hi` is clamped to `nbits`, and `nbits` need not be a multiple of 64 - the caller
/// here always has one, but the fuzzer deliberately does not, since a size that
/// happens to be exact hides the last word's masking.
///
/// The trick is that a candidate range is expressed by **setting** the bits outside
/// it: a bit that is not a candidate is indistinguishable from a bit that is
/// allocated, so the two exclusions and the allocation test become one `!= u64::MAX`.
#[inline]
pub fn find_in(words: &[u64], nbits: usize, lo: usize, hi: usize) -> Option<usize> {
    let hi = hi.min(nbits);
    if lo >= hi {
        return None;
    }
    let first = lo / 64;
    let last = (hi - 1) / 64;
    let mut w = first;
    while w <= last && w < words.len() {
        let mut v = words[w];
        if w == first {
            // Bits below `lo` are not candidates.
            v |= low_mask(lo - w * 64);
        }
        if w == last {
            // Bits at or above `hi` are not candidates. Both arms can apply to the
            // same word, which is the single-word range.
            v |= !low_mask(hi - w * 64);
        }
        if v != u64::MAX {
            return Some(w * 64 + (!v).trailing_zeros() as usize);
        }
        w += 1;
    }
    None
}

/// The lowest free bit at or after `from`, wrapping once - the rotating-hint search.
///
/// Exactly the old cyclic loop's answer: the first free bit in `[from, nbits)`, else
/// the first free bit in `[0, from)`. Expressed as two [`find_in`] calls rather than
/// as its own wrapping scan, because a wrapping word walk needs the masking logic a
/// second time and the whole point of this module is to have one copy of it.
#[inline]
pub fn find_from(words: &[u64], nbits: usize, from: usize) -> Option<usize> {
    if nbits == 0 {
        return None;
    }
    let from = if from >= nbits { 0 } else { from };
    match find_in(words, nbits, from, nbits) {
        Some(i) => Some(i),
        None => find_in(words, nbits, 0, from),
    }
}

/// The start of the lowest run of `n` free bits in `[0, nbits)`, or `None`.
///
/// The contiguous path ([`crate::mm::frames::alloc_contig`]). First fit, and the run
/// may straddle any number of words. Kept here beside its siblings so all three
/// searches share one set of boundary conventions and one fuzzer, even though this
/// one runs only at bring-up.
///
/// Still a bit-at-a-time walk, deliberately: a run search cannot skip an all-free
/// word without tracking a partial run across the skip, and the extra state is the
/// kind of thing this module exists to keep out of `frames.rs`. It runs a handful of
/// times per boot, so the win would be unmeasurable and the risk would not.
#[inline]
pub fn find_run(words: &[u64], nbits: usize, n: usize) -> Option<usize> {
    if n == 0 || n > nbits {
        return None;
    }
    let mut run = 0usize;
    for i in 0..nbits {
        let (w, b) = (i / 64, i % 64);
        if w >= words.len() {
            return None;
        }
        if words[w] & (1u64 << b) != 0 {
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
