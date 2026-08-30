//! Vectorised bulk float math, dispatched by ELF ifunc (docs/TILES.md,
//! docs/LIBRHEO.md "ifunc").
//!
//! # Why a separate module from [`super::fmath`]
//!
//! `fmath.rs` is `#[path]`-included **verbatim** by three other builds - the
//! kernel's compute engine, `bench-core`, and the `tilelinux` static-glibc Linux
//! fixture that exists to prove the two substrates agree bit for bit
//! (docs/TILES.md 13.4b). It therefore has to stay dependency-free: it cannot
//! name `crate::ifunc`, `crate::sys`, or anything else librheo provides. So the
//! scalar reference stays there and the dispatch lives here, where only a
//! librheo cell compiles it.
//!
//! # What is dispatched
//!
//! [`exp2f_into`] - `dst[i] = 2^src[i]` over a slice. A *bulk* kernel rather
//! than a faster scalar `exp2f`, because that is where the win is: the scalar
//! function is already ~15 branch-free arithmetic operations, and an ifunc on it
//! would only ever pick between scalar implementations. Eight lanes at a time is
//! a different quantity of work, and the attention softmax evaluates it once per
//! score.
//!
//! # Bit-exactness with the scalar reference
//!
//! The vector tiers are **bit-identical** to [`super::fmath::exp2f`] on every
//! finite input, and this is a design constraint rather than a happy result -
//! `librheotilebattle`'s FlashAttention oracles compare tilings against each
//! other, so an exp that differed by an ulp between tiers would surface as a
//! tiling bug. What that costs:
//!
//! - **No FMA.** Rust does not contract `a * b + c` into an FMA (there is no
//!   fast-math), so the scalar Horner chain is separate multiplies and adds; the
//!   vector tier must use separate `_mm256_mul_ps` / `_mm256_add_ps` too. An FMA
//!   version would be faster *and* more accurate, and would not match.
//! - **The NaN path is the one exception**, and it is handled explicitly rather
//!   than left to luck. `f32 as i32` saturates in Rust, so scalar `exp2f(NaN)`
//!   rounds to `n = 0` and propagates NaN through the polynomial; `vcvttps2dq`
//!   instead yields `i32::MIN`, whose exponent field would take the `e <= 0`
//!   branch and return `0.0`. The tier therefore selects the input back over the
//!   result for unordered lanes. Equality for NaN is asserted as *is-NaN*, not
//!   as bit-identical payload - a signalling NaN is quieted by the scalar
//!   arithmetic and passed through here.
//!
//! # Why the vector tier is gated on `sse2`, not on `target_arch`
//!
//! librheo is compiled for **two** x86-64 targets: the hard-float cell target
//! (`targets/rheo_cell-x86_64.json`, `+sse,+sse2`) that a loaded cell runs as,
//! and the bare `x86_64-unknown-none` (`-sse,-sse2,+soft-float`) that the
//! kernel-side and `net` builds use. `__m256` is a *float* vector, and on a
//! soft-float target LLVM cannot legalise one:
//!
//!   rustc-LLVM ERROR: Do not know how to split the result of this operator!
//!
//! That is a hard compiler crash, not a diagnostic. `tile::simd` does not hit it
//! only because its GEMM tiers are `__m256i` (integer). So the gate is
//! `target_feature = "sse2"`, which is precisely the property that differs
//! between the two targets, rather than `target_arch`, which does not.

/// `dst[i] = 2^src[i]`, vectorised where the hardware allows.
///
/// Bit-identical to `fmath::exp2f` per element on finite inputs, whichever tier
/// the ifunc resolved to. Copies `min(src.len(), dst.len())` elements.
pub fn exp2f_into(src: &[f32], dst: &mut [f32]) {
    let n = core::cmp::min(src.len(), dst.len());
    // SAFETY: `n` is bounded by both slices' lengths, and the two are distinct
    // borrows so the ranges cannot overlap.
    unsafe { rheo_exp2f_into(src.as_ptr(), dst.as_mut_ptr(), n) }
}

// The ifunc. Call sites reach `rheo_exp2f_into` as an ordinary `extern "C"`
// function; the linker routes them through one GOT slot and emits the
// `R_*_IRELATIVE` relocation that `crate::ifunc::apply_irel` fills at crt0.
crate::ifunc!(rheo_exp2f_into => rheo_exp2f_resolve);

unsafe extern "C" {
    fn rheo_exp2f_into(src: *const f32, dst: *mut f32, n: usize);
}

/// Tier codes, shared with [`tier_name`] and the selection report.
pub const SCALAR: u8 = 0;
/// x86 AVX2 - 8 lanes.
pub const AVX2: u8 = 1;

/// Which tier the resolver chose, recorded for reporting. Written once, by the
/// resolver, before `main`.
static CHOSEN: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(SCALAR);

/// The tier `rheo_exp2f_into` resolved to. Valid after crt0.
pub fn tier() -> u8 {
    CHOSEN.load(core::sync::atomic::Ordering::Relaxed)
}

/// Human-readable tier name.
pub fn tier_name(t: u8) -> &'static str {
    match t {
        AVX2 => "avx2",
        _ => "scalar",
    }
}

// ------------------------------------------------------------------ the tiers

/// Portable tier: the scalar reference, once per element. Also the correctness
/// oracle every other tier is checked against.
///
/// # Safety
/// `src` and `dst` are valid for `n` `f32`s and do not overlap.
#[unsafe(no_mangle)]
unsafe extern "C" fn rheo_exp2f_scalar(src: *const f32, dst: *mut f32, n: usize) {
    for i in 0..n {
        // SAFETY: caller's contract.
        unsafe { dst.add(i).write(super::fmath::exp2f(src.add(i).read())) }
    }
}

/// AVX2 tier: 8 lanes of the identical arithmetic.
///
/// AVX-512 is deliberately absent rather than written and unproven: QEMU's TCG
/// exposes AVX2 but not AVX-512 (docs/TILES.md 4), so a 16-lane tier could not be
/// gated by the resolver's own correctness check on any machine in this tree, and
/// shipping a vector kernel whose first execution is on a customer's hardware is
/// the untested claim docs/ENGINEERING.md 7 refuses. The shape below is the one it
/// would take.
///
/// # Safety
/// AVX2 is present (the resolver checked); `src`/`dst` valid for `n` and
/// non-overlapping.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[target_feature(enable = "avx2")]
unsafe fn exp2f_avx2(src: *const f32, dst: *mut f32, n: usize) {
    use super::fmath;
    use core::arch::x86_64::*;
    unsafe {
        let c1 = _mm256_set1_ps(fmath::C1);
        let c2 = _mm256_set1_ps(fmath::C2);
        let c3 = _mm256_set1_ps(fmath::C3);
        let c4 = _mm256_set1_ps(fmath::C4);
        let c5 = _mm256_set1_ps(fmath::C5);
        let c6 = _mm256_set1_ps(fmath::C6);
        let one = _mm256_set1_ps(1.0);
        let half = _mm256_set1_ps(0.5);
        let signmask = _mm256_set1_ps(-0.0);
        let inf = _mm256_set1_ps(f32::INFINITY);
        let zero = _mm256_setzero_ps();

        let mut i = 0usize;
        while i + 8 <= n {
            let x = _mm256_loadu_ps(src.add(i));

            // Nearest integer, ties away from zero: add 0.5 carrying x's sign,
            // then truncate - exactly the scalar `(x + 0.5) as i32` /
            // `(x - 0.5) as i32` split, without the branch.
            let nudge = _mm256_or_ps(half, _mm256_and_ps(x, signmask));
            let ni = _mm256_cvttps_epi32(_mm256_add_ps(x, nudge));
            let r = _mm256_sub_ps(x, _mm256_cvtepi32_ps(ni));

            // Horner, separate mul/add so it matches the scalar chain exactly.
            let mut p = _mm256_add_ps(c5, _mm256_mul_ps(r, c6));
            p = _mm256_add_ps(c4, _mm256_mul_ps(r, p));
            p = _mm256_add_ps(c3, _mm256_mul_ps(r, p));
            p = _mm256_add_ps(c2, _mm256_mul_ps(r, p));
            p = _mm256_add_ps(c1, _mm256_mul_ps(r, p));
            p = _mm256_add_ps(one, _mm256_mul_ps(r, p));

            // scalb(p, n): build 2^n from the exponent field.
            let e = _mm256_add_epi32(ni, _mm256_set1_epi32(127));
            let scale = _mm256_castsi256_ps(_mm256_slli_epi32(e, 23));
            let mut res = _mm256_mul_ps(p, scale);

            // The guards, in the scalar function's order: scalb's underflow /
            // overflow first, then exp2f's own domain ends.
            let ef = _mm256_castsi256_ps(e);
            let e_le0 = _mm256_castsi256_ps(_mm256_cmpgt_epi32(_mm256_set1_epi32(1), e));
            let e_ge255 = _mm256_castsi256_ps(_mm256_cmpgt_epi32(e, _mm256_set1_epi32(254)));
            let _ = ef;
            res = _mm256_blendv_ps(res, zero, e_le0);
            res = _mm256_blendv_ps(res, inf, e_ge255);
            res = _mm256_blendv_ps(
                res,
                inf,
                _mm256_cmp_ps(x, _mm256_set1_ps(128.0), _CMP_GE_OQ),
            );
            res = _mm256_blendv_ps(
                res,
                zero,
                _mm256_cmp_ps(x, _mm256_set1_ps(-150.0), _CMP_LE_OQ),
            );
            // NaN in, NaN out (module docs): `vcvttps2dq` maps NaN to i32::MIN,
            // which would otherwise take the `e <= 0` branch and yield 0.0.
            res = _mm256_blendv_ps(res, x, _mm256_cmp_ps(x, x, _CMP_UNORD_Q));

            _mm256_storeu_ps(dst.add(i), res);
            i += 8;
        }
        // Tail: the scalar reference, so a short slice is not a second code path
        // that could disagree.
        while i < n {
            dst.add(i).write(fmath::exp2f(src.add(i).read()));
            i += 1;
        }
    }
}

/// `extern "C"` shim so the AVX2 tier has an address the resolver can return.
///
/// # Safety
/// As [`exp2f_avx2`].
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[unsafe(no_mangle)]
unsafe extern "C" fn rheo_exp2f_avx2(src: *const f32, dst: *mut f32, n: usize) {
    // SAFETY: only reachable through the GOT slot the resolver filled, and the
    // resolver fills it only after observing AVX2 present and correct.
    unsafe { exp2f_avx2(src, dst, n) }
}

// --------------------------------------------------------------- the resolver

/// Inputs the correctness gate evaluates. Chosen to reach every branch of the
/// scalar function: both signs of the rounding nudge, a tie, the `e <= 0`
/// underflow, the `e >= 255` overflow, and both domain guards. 17 elements, so
/// the vector body runs twice and the scalar tail once - a tier that got the
/// tail wrong fails here rather than on a user's odd-length row.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
const GATE: [f32; 17] = [
    0.0, 1.0, -1.0, 0.5, -0.5, 3.25, -3.25, 0.125, -0.125, 12.5, -12.5, 127.9, -149.5, 128.0,
    -150.0, -126.0, 63.75,
];

/// Choose the implementation `rheo_exp2f_into` resolves to.
///
/// Runs before the heap exists, so: no allocation, no `alloc` types, and no call
/// to any ifunc-dispatched symbol. Fixed stack arrays only.
///
/// Beyond glibc's feature check this **observes the tier being right**
/// (docs/ENGINEERING.md 1): AVX2 is selected only after producing bit-identical
/// output to the scalar reference on [`GATE`]. A feature bit says the
/// instruction exists; it does not say this use of it is correct, and a wrong
/// vector exp would surface as a subtly wrong attention row rather than a fault.
#[unsafe(no_mangle)]
extern "C" fn rheo_exp2f_resolve() -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        if crate::sys::cpu_features().simd & crate::sys::SIMD_AVX2 != 0 {
            let mut want = [0f32; GATE.len()];
            let mut got = [0f32; GATE.len()];
            // SAFETY: both buffers are `GATE.len()` long and distinct locals.
            unsafe {
                rheo_exp2f_scalar(GATE.as_ptr(), want.as_mut_ptr(), GATE.len());
                rheo_exp2f_avx2(GATE.as_ptr(), got.as_mut_ptr(), GATE.len());
            }
            let mut ok = true;
            let mut i = 0;
            while i < GATE.len() {
                // Bit comparison, not `==`: `==` is false for NaN and true for
                // `0.0 == -0.0`, and this is asserting the two produced the
                // *same value*. NaN is compared as is-NaN (module docs).
                let (a, b) = (want[i], got[i]);
                let same = if a.is_nan() {
                    b.is_nan()
                } else {
                    a.to_bits() == b.to_bits()
                };
                if !same {
                    ok = false;
                }
                i += 1;
            }
            if ok {
                CHOSEN.store(AVX2, core::sync::atomic::Ordering::Relaxed);
                return rheo_exp2f_avx2 as *const () as usize;
            }
        }
    }
    CHOSEN.store(SCALAR, core::sync::atomic::Ordering::Relaxed);
    rheo_exp2f_scalar as *const () as usize
}
