//! Runtime-dispatched SIMD tile kernels with a boot probe (docs/TILES.md 4).
//!
//! The scalar tile kernels (`super::kernels`) are the correctness reference and
//! the portable fallback. On x86 a hard-float cell can also run AVX2 / AVX-512
//! variants of the int8 GEMM block; this module picks the best one **at run
//! time** and only after proving it correct, so the same binary:
//!
//!   - uses the widest vector unit the hardware actually has (queried from the
//!     kernel's validated feature report, `sys::cpu_features`), and
//!   - falls back gracefully - a tier that is absent, or whose result does not
//!     match the scalar kernel bit-for-bit, is dropped.
//!
//! The choice is made by `probe()`: for each tier the hardware advertises it
//! runs a **functionality test** (a fixed GEMM whose result must equal the
//! scalar kernel's, bit-for-bit - the on-boot form of the `comparison/tiles`
//! differential fuzz) and a **micro-benchmark** (`sys::cycles`), then selects
//! the fastest tier that passed. Under QEMU icount the benchmark counts
//! instructions, so the wider tier - which executes fewer - wins deterministic-
//! ally; on real hardware it is wall-clock. Honest: the benchmark's *timing* is
//! representative only on hardware (QEMU models no caches, docs/TILES.md 4); the
//! functionality test is meaningful everywhere.
//!
//! All tiers are compiled in unconditionally (a `#[target_feature]` function
//! always emits its codegen); the dispatch selects one only when the feature is
//! present. AVX-512-VNNI (the int8 `dpbusd` dot-product) needs the packed-B
//! dot-product layout and is proven bit-exact on real hardware in
//! `comparison/tiles`; the tile executor's strided-B AXPY shape uses the AVX2 /
//! AVX-512 widening-multiply path here.

use core::sync::atomic::{AtomicU8, Ordering};

const UNPROBED: u8 = 0xFF;
/// Scalar (portable fallback / correctness reference).
pub const SCALAR: u8 = 0;
/// x86 AVX2 (x86-64-v3).
pub const AVX2: u8 = 1;
/// x86 AVX-512F (x86-64-v4).
pub const AVX512: u8 = 2;

static TIER: AtomicU8 = AtomicU8::new(UNPROBED);
/// Bitmask (1 << tier) of tiers that PASSED the functionality check - i.e.
/// produced bit-identical output to scalar on-OS. Distinct from the *selected*
/// tier (`TIER`), which the benchmark chooses by speed. Under emulation the
/// benchmark may pick scalar (TCG models no SIMD speedup), but a wider tier can
/// still be proven correct here - that is what a caller asserts to show the
/// hardware path genuinely ran (docs/TILES.md 4).
static FUNC_MASK: AtomicU8 = AtomicU8::new(0);

/// Human-readable name of a tier code.
pub fn tier_name(t: u8) -> &'static str {
    match t {
        AVX2 => "avx2",
        AVX512 => "avx512",
        _ => "scalar",
    }
}

/// Bitmask `(1 << tier)` of tiers that passed the on-OS functionality check
/// (bit-exact vs scalar). Bit `1 << SCALAR` is always set. Valid after `tier()`
/// or `probe()` has run.
pub fn functional_tiers() -> u8 {
    FUNC_MASK.load(Ordering::Relaxed)
}

/// The selected SIMD tier, probing once on first use (idempotent; single-vcore
/// cell, so the store races nothing).
pub fn tier() -> u8 {
    let t = TIER.load(Ordering::Relaxed);
    if t != UNPROBED {
        return t;
    }
    let sel = probe();
    TIER.store(sel, Ordering::Relaxed);
    sel
}

/// Probe the hardware and choose a tier: functionality-gate each advertised
/// tier (bit-exact vs scalar), micro-benchmark the survivors, pick the fastest.
/// Always returns a valid tier (SCALAR at worst). Re-runnable.
pub fn probe() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        let simd = crate::sys::cpu_features().simd;
        let mut func = 1u8 << SCALAR; // scalar always passes
        let mut best = SCALAR;
        let mut best_cyc = bench_tier(SCALAR);
        // A tier is eligible only if the hardware advertises it AND it produces
        // bit-identical output to scalar on-OS (functionality). Among the
        // eligible, the fastest by micro-benchmark wins. Under emulation the
        // benchmark may keep scalar (TCG has no SIMD speedup) - honest; the
        // functionality mask still records that the wider path ran correctly.
        if simd & crate::sys::SIMD_AVX2 != 0 && functional(AVX2) {
            func |= 1 << AVX2;
            let c = bench_tier(AVX2);
            if c < best_cyc {
                best = AVX2;
                best_cyc = c;
            }
        }
        if simd & crate::sys::SIMD_AVX512F != 0 && functional(AVX512) {
            func |= 1 << AVX512;
            let c = bench_tier(AVX512);
            if c < best_cyc {
                best = AVX512;
                best_cyc = c;
            }
        }
        let _ = best_cyc;
        FUNC_MASK.store(func, Ordering::Relaxed);
        best
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // ARM64 (NEON) and RISC-V (scalar F/D) auto-vectorise the scalar kernel
        // at their hard-float baseline; no hand-written wider tier here.
        FUNC_MASK.store(1 << SCALAR, Ordering::Relaxed);
        SCALAR
    }
}

/// Accumulating int8 GEMM block dispatched to the selected tier - bit-identical
/// to `super::kernels::gemm_i8_i32` on every tier (the probe guarantees it).
///
/// # Safety
/// Same contract as `super::kernels::gemm_i8_i32`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn gemm_i8_i32(
    a: *const i8,
    as_: usize,
    b: *const i8,
    bs: usize,
    c: *mut i32,
    cs: usize,
    m: usize,
    n: usize,
    k: usize,
) {
    unsafe { run_tier(tier(), a, as_, b, bs, c, cs, m, n, k) }
}

/// Run a specific tier (used by the dispatch and by the probe's checks).
///
/// # Safety
/// The tier's feature must be present (the probe only calls a tier it detected).
#[allow(clippy::too_many_arguments)]
unsafe fn run_tier(
    tier: u8,
    a: *const i8,
    as_: usize,
    b: *const i8,
    bs: usize,
    c: *mut i32,
    cs: usize,
    m: usize,
    n: usize,
    k: usize,
) {
    unsafe {
        match tier {
            #[cfg(target_arch = "x86_64")]
            AVX2 => gemm_avx2(a, as_, b, bs, c, cs, m, n, k),
            #[cfg(target_arch = "x86_64")]
            AVX512 => gemm_avx512(a, as_, b, bs, c, cs, m, n, k),
            _ => super::kernels::gemm_i8_i32(a, as_, b, bs, c, cs, m, n, k),
        }
    }
}

// ---------------------------------------------------------------- probe helpers

/// Deterministic i8 fill in [-3, 3] (includes 0, to exercise the `av == 0` skip,
/// and negatives, to exercise sign-extension) - a fixed xorshift so the probe is
/// reproducible.
#[cfg(target_arch = "x86_64")]
fn fill(n: usize) -> alloc::vec::Vec<i8> {
    let mut v = alloc::vec::Vec::with_capacity(n);
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.push(((s % 7) as i8) - 3);
    }
    v
}

/// True if `tier` produces bit-identical output to the scalar kernel on a fixed
/// GEMM with odd dimensions (so the SIMD main loop AND the scalar tail both run).
#[cfg(target_arch = "x86_64")]
fn functional(tier: u8) -> bool {
    let (m, n, k) = (7usize, 19usize, 11usize);
    let a = fill(m * k);
    let b = fill(k * n);
    let mut c_ref = alloc::vec![0i32; m * n];
    let mut c_test = alloc::vec![0i32; m * n];
    unsafe {
        super::kernels::gemm_i8_i32(a.as_ptr(), k, b.as_ptr(), n, c_ref.as_mut_ptr(), n, m, n, k);
        run_tier(
            tier,
            a.as_ptr(),
            k,
            b.as_ptr(),
            n,
            c_test.as_mut_ptr(),
            n,
            m,
            n,
            k,
        );
    }
    c_ref == c_test
}

/// Total cycles for a few reps of a fixed-size block on `tier` (relative cost;
/// under QEMU icount this is an instruction count).
#[cfg(target_arch = "x86_64")]
fn bench_tier(tier: u8) -> u64 {
    let (m, n, k) = (32usize, 32usize, 32usize);
    let a = fill(m * k);
    let b = fill(k * n);
    let mut c = alloc::vec![0i32; m * n];
    let t0 = crate::sys::cycles();
    for _ in 0..8 {
        unsafe {
            run_tier(
                tier,
                a.as_ptr(),
                k,
                b.as_ptr(),
                n,
                c.as_mut_ptr(),
                n,
                m,
                n,
                k,
            )
        };
    }
    let t1 = crate::sys::cycles();
    core::hint::black_box(&c);
    t1.wrapping_sub(t0)
}

// -------------------------------------------------------------- x86 SIMD kernels
//
// Both vectorise the innermost `j` loop of the AXPY-shaped scalar kernel: for
// each output row i and each k index p, broadcast A[i][p] and multiply-add it
// across a contiguous run of B[p][j] into C[i][j]. Integer `mullo`/`add` wrap
// exactly like the scalar `*`/`+=` in a release build, so the result is
// bit-identical (the probe's functionality test asserts it).

/// AVX2 (x86-64-v3): 8 int32 lanes. Widens 8 int8 of B via `cvtepi8_epi32`,
/// multiplies by the broadcast A element, accumulates into C.
///
/// # Safety
/// Caller guarantees AVX2 is present; buffers valid per `gemm_i8_i32`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn gemm_avx2(
    a: *const i8,
    as_: usize,
    b: *const i8,
    bs: usize,
    c: *mut i32,
    cs: usize,
    m: usize,
    n: usize,
    k: usize,
) {
    use core::arch::x86_64::*;
    unsafe {
        for i in 0..m {
            let arow = a.add(i * as_);
            let crow = c.add(i * cs);
            for p in 0..k {
                let av = *arow.add(p) as i32;
                if av == 0 {
                    continue;
                }
                let avv = _mm256_set1_epi32(av);
                let brow = b.add(p * bs);
                let mut j = 0;
                while j + 8 <= n {
                    let b8 = _mm256_cvtepi8_epi32(_mm_loadl_epi64(brow.add(j) as *const __m128i));
                    let cv = _mm256_loadu_si256(crow.add(j) as *const __m256i);
                    let acc = _mm256_add_epi32(cv, _mm256_mullo_epi32(avv, b8));
                    _mm256_storeu_si256(crow.add(j) as *mut __m256i, acc);
                    j += 8;
                }
                while j < n {
                    *crow.add(j) += av * (*brow.add(j) as i32);
                    j += 1;
                }
            }
        }
    }
}

/// AVX-512F (x86-64-v4): 16 int32 lanes, same shape as `gemm_avx2`.
///
/// # Safety
/// Caller guarantees AVX-512F is present; buffers valid per `gemm_i8_i32`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
#[allow(clippy::too_many_arguments)]
unsafe fn gemm_avx512(
    a: *const i8,
    as_: usize,
    b: *const i8,
    bs: usize,
    c: *mut i32,
    cs: usize,
    m: usize,
    n: usize,
    k: usize,
) {
    use core::arch::x86_64::*;
    unsafe {
        for i in 0..m {
            let arow = a.add(i * as_);
            let crow = c.add(i * cs);
            for p in 0..k {
                let av = *arow.add(p) as i32;
                if av == 0 {
                    continue;
                }
                let avv = _mm512_set1_epi32(av);
                let brow = b.add(p * bs);
                let mut j = 0;
                while j + 16 <= n {
                    let b16 = _mm512_cvtepi8_epi32(_mm_loadu_si128(brow.add(j) as *const __m128i));
                    let cv = _mm512_loadu_si512(crow.add(j) as *const _);
                    let acc = _mm512_add_epi32(cv, _mm512_mullo_epi32(avv, b16));
                    _mm512_storeu_si512(crow.add(j) as *mut _, acc);
                    j += 16;
                }
                while j < n {
                    *crow.add(j) += av * (*brow.add(j) as i32);
                    j += 1;
                }
            }
        }
    }
}
