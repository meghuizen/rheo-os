//! Float math for tile kernels: the small set of transcendental functions a
//! softmax needs, from scratch (docs/TILES.md).
//!
//! ## Why this exists rather than a dependency
//!
//! FlashAttention's online softmax needs `exp`, and nothing in this tree provided
//! it: `core` has no transcendentals, and a cell has no `std`. The obvious answer is
//! the `libm` crate - pure Rust, `no_std`, and it builds on all three ISAs, so it
//! would clear the docs/SUBSTRATE.md 11 Tier S bar.
//!
//! It is not taken, for a reason specific to what is needed here. `libm` provides
//! *correctly rounded* `expf` over the whole domain, and a softmax needs neither
//! half of that: its argument is always `x - rowmax`, i.e. **non-positive and
//! bounded**, and its result is immediately divided by a sum of such results, so a
//! relative error of a few 1e-7 is invisible in the answer. What a tile kernel does
//! need is to be *inlineable and vectorisable* - the whole point of the tile
//! framework is that the inner loop lowers to SIMD (`tile::simd`), and a call into
//! an opaque crate function per element defeats that. So this is ~40 lines with a
//! stated error bound, and the bound is asserted against hand-computed values rather
//! than assumed.
//!
//! ## exp2 as the primitive
//!
//! [`exp2f`] is the primitive and [`expf`] is `exp2f(x * log2(e))`, which is the
//! order real attention kernels use: the scaling by `log2(e)` folds into the
//! attention score scale (`1/sqrt(d) * log2(e)`) and costs nothing at all, while
//! `2^n` is exact by exponent-field arithmetic. Doing it the other way round -
//! `exp` as the primitive - would need a `ln2` range reduction per element and the
//! `2^n` scaling anyway.
//!
//! ## Accuracy, stated
//!
//! Range reduction `x = n + r` with `n` the nearest integer, so `|r| <= 0.5`, then a
//! degree-6 Taylor series for `2^r = e^(r ln2)`. The truncation error of the series
//! at `|r| = 0.5` is `(0.5 ln2)^7 / 5040 ~= 1.3e-8` relative, below `f32`'s `6e-8`
//! epsilon, so the result is accurate to about one ulp across the reduced range and
//! the total error is dominated by the `f32` rounding of the polynomial itself. The
//! proof asserts a **relative** bound of `2e-7` against exact values, which is ~3
//! ulps - loose enough not to be a rounding-order tripwire, tight enough that a
//! wrong coefficient fails it.

/// `log2(e)`, for folding an `exp` into [`exp2f`].
///
/// `core`'s own constant, not a literal: it is bit-identical here (`0x3fb8aa3b`,
/// checked) and saying which constant this is beats spelling out digits a reader
/// has to recognise.
pub const LOG2_E: f32 = core::f32::consts::LOG2_E;

// Taylor coefficients of `2^r = e^(r ln2) = sum (r ln2)^k / k!`, k = 1..6.
//
// `C1` is `ln2` itself, so it is named as `core`'s constant. `C2..C6` stay decimal
// literals rather than being computed from it: `const` evaluation of a power series
// would produce the same numbers by a route a reader has to re-derive, and these are
// the numbers the accuracy claim above is about. Every one is the shortest literal
// that round-trips to its `f32`, so the value is exactly what is written.
pub const C1: f32 = core::f32::consts::LN_2; // ln2 (bit-identical to 0x3f317218)
pub const C2: f32 = 0.240_226_5; // ln2^2 / 2
pub const C3: f32 = 0.055_504_11; // ln2^3 / 6
pub const C4: f32 = 0.009_618_129; // ln2^4 / 24
pub const C5: f32 = 0.001_333_356; // ln2^5 / 120
pub const C6: f32 = 0.000_154_035; // ln2^6 / 720

/// `2^x` for `f32`.
///
/// Saturating rather than trapping at the edges: `x <= -150` gives `0.0` (below the
/// smallest subnormal) and `x >= 128` gives `+inf`, which are the values the
/// continuous function tends to. A softmax never reaches either - its argument is
/// `<= 0` and the interesting range is `[-30, 0]` - but a kernel that silently
/// produced a NaN for an out-of-range score would turn one bad input into an
/// all-NaN output row, so the ends are defined.
///
/// NaN in gives NaN out (the comparisons below are all false, and the polynomial
/// propagates it), which is the right answer and not a special case.
#[inline]
pub fn exp2f(x: f32) -> f32 {
    if x >= 128.0 {
        return f32::INFINITY;
    }
    if x <= -150.0 {
        return 0.0;
    }
    // Nearest integer, ties away from zero. `as i32` truncates, so the 0.5 nudge is
    // what makes it rounding - and it must be a *nudge toward* the sign, not
    // `+ 0.5`, or negative x would round the wrong way.
    let n = if x >= 0.0 {
        (x + 0.5) as i32
    } else {
        (x - 0.5) as i32
    };
    let r = x - n as f32;
    // Horner, so the polynomial is 6 multiply-adds and vectorises as one chain.
    let p = 1.0 + r * (C1 + r * (C2 + r * (C3 + r * (C4 + r * (C5 + r * C6)))));
    scalb(p, n)
}

/// `e^x` for `f32`.
#[inline]
pub fn expf(x: f32) -> f32 {
    exp2f(x * LOG2_E)
}

/// `y * 2^n`, by adding `n` to the exponent field.
///
/// Exact for any `n` that keeps the result normal, which is the only case the callers
/// reach (the saturating guards in [`exp2f`] bound `n` to `-150..128` and `y` to
/// `[0.7, 1.42]`). Below the normal range it returns 0 rather than constructing a
/// subnormal by shifting: gradual underflow would be more accurate, and it is
/// deliberately not done, because a softmax weight below `2^-126` cannot affect a sum
/// that contains a weight of 1 - the `f32` result would be identical - so the
/// accuracy would be unobservable and the code would be longer.
#[inline]
fn scalb(y: f32, n: i32) -> f32 {
    let e = n + 127;
    if e <= 0 {
        return 0.0;
    }
    if e >= 255 {
        return f32::INFINITY;
    }
    y * f32::from_bits((e as u32) << 23)
}

/// Signature of a bulk in-place `2^x` kernel: `n` floats at `p`, each replaced by
/// `2^` itself. `extern "C"` because the implementation that replaces the default
/// is reached as an address rather than by name (see [`set_exp2f_inplace`]).
pub type Exp2fInplaceFn = unsafe extern "C" fn(*mut f32, usize);

/// The installed bulk kernel, or 0 for "use the scalar loop below".
///
/// This indirection exists because of a build constraint, not a design
/// preference. This file is `#[path]`-included **verbatim** by builds that have
/// no librheo - the `tilelinux` static-glibc fixture among them - so it cannot
/// name a librheo dispatch, and [`super::attn`] (included by the same builds)
/// cannot either. A pointer that defaults to the scalar path lets the shared
/// recurrence call one function while each build supplies what it has: nothing
/// at all for the verbatim includers, and the ifunc-resolved vector kernel for a
/// librheo cell (`tile::fsimd::install`).
///
/// The cost is one relaxed load and an indirect call **per K-block**, not per
/// element, because what is installed is a *bulk* kernel - which is the whole
/// reason the bulk form exists.
static EXP2F_INPLACE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install a bulk `2^x` kernel for [`exp2f_inplace`] to use.
///
/// The installed kernel **must** be bit-identical to [`exp2f`] per element on
/// finite inputs, and NaN-for-NaN. It is not a place to trade accuracy for
/// speed: the FlashAttention oracles compare tilings of the same problem against
/// each other, so a kernel that differed by an ulp would surface as a tiling bug
/// rather than as an accuracy question.
///
/// Called once at process start, before any attention kernel runs.
pub fn set_exp2f_inplace(f: Exp2fInplaceFn) {
    EXP2F_INPLACE.store(f as usize, core::sync::atomic::Ordering::Relaxed);
}

/// `xs[i] = 2^xs[i]`, through the installed bulk kernel if there is one.
///
/// Bit-identical to calling [`exp2f`] on each element either way - that is the
/// contract [`set_exp2f_inplace`] imposes on what it accepts.
#[inline]
pub fn exp2f_inplace(xs: &mut [f32]) {
    let p = EXP2F_INPLACE.load(core::sync::atomic::Ordering::Relaxed);
    if p != 0 {
        // SAFETY: `p` was installed by `set_exp2f_inplace` from a fn item of
        // this exact signature, and `xs` is a valid mutable slice of `len`.
        unsafe {
            let f: Exp2fInplaceFn = core::mem::transmute(p);
            f(xs.as_mut_ptr(), xs.len());
        }
        return;
    }
    for x in xs.iter_mut() {
        *x = exp2f(*x);
    }
}

/// The largest of `xs`, or `-inf` for an empty slice.
///
/// Here rather than as an iterator chain because `f32` is not `Ord`, so the obvious
/// `.max()` does not exist, and each caller writing its own fold is how a NaN
/// handling difference creeps in between two of them. This one propagates a
/// `-inf` start and lets NaN lose every comparison, so a NaN score does not
/// silently become the row max and zero the whole row.
#[inline]
pub fn rowmax(xs: &[f32]) -> f32 {
    let mut m = f32::NEG_INFINITY;
    for &x in xs {
        if x > m {
            m = x;
        }
    }
    m
}
