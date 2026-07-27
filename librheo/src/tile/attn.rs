//! **FlashAttention 2 and 3** as tile programs (docs/TILES.md).
//!
//! ## What attention costs, and what FlashAttention changes
//!
//! Scaled dot-product attention for one head is
//! `O = softmax(Q K^T / sqrt(d)) V`, with `Q` of shape `[Tq, d]`, `K`/`V` of
//! `[Tk, d]`. Written directly it **materialises `S = Q K^T`**, which is `Tq x Tk` -
//! quadratic in sequence length and, for any real context window, far larger than
//! the inputs and outputs put together. At `Tq = Tk = 8192` that is 67 M f32 = 256
//! MiB for a matrix nothing wants to keep.
//!
//! FlashAttention's insight is that softmax does not actually need the whole row at
//! once. Its normaliser is a *sum*, and a sum can be accumulated - provided the
//! running maximum used for numerical stability is corrected as it moves. So the
//! kernel walks `K`/`V` in blocks, keeping three running quantities per query row:
//! the max `m`, the sum `l`, and the unnormalised output `acc`. Nothing of size
//! `Tq x Tk` is ever stored, and memory traffic drops from quadratic to linear.
//!
//! ## Online softmax, exactly
//!
//! For a block with scores `s`, let `m' = max(m, max(s))` and `c = exp(m - m')`.
//! Then
//!
//! ```text
//!   l   <- l * c + sum_j exp(s_j - m')
//!   acc <- acc * c + sum_j exp(s_j - m') * V_j
//! ```
//!
//! and after the last block `O = acc / l`. The rescale by `c` is what makes this
//! **algebraically identical** to computing the whole row at once: every term ends
//! up divided by `exp(m_final)`, whichever block first raised the max. That is the
//! load-bearing property, and it is what the proof asserts - the result must not
//! depend on the block size, because the block size is a tiling decision and tiling
//! decisions may not change answers (docs/TILES.md).
//!
//! It is *not* bit-identical to the naive form, and claiming otherwise would be
//! false: the additions happen in a different order, so the two differ by
//! floating-point rounding. The proof therefore asserts a **relative** bound, which
//! is the honest shape of the claim (docs/ENGINEERING.md 7).
//!
//! ## FA2 versus FA3, in this framework
//!
//! The two are not different algorithms. FA2 is the loop structure above - the
//! rescale moved out of the inner loop and onto the accumulator, which is what
//! distinguishes FA2 from FA1. FA3 is the same arithmetic **pipelined**: while one
//! stage computes on block `i`, another stages block `i+1`, so data movement overlaps
//! compute instead of alternating with it. On a GPU that is warp specialisation; here
//! it is [`flash_attention_3`]: two strands over a double-buffered staging pair,
//! handing off at a tile boundary.
//!
//! Honest about what that buys *here*: the runtime is cooperative on one CPU, so the
//! producer and consumer interleave rather than run at once, and FA3's wall-clock win
//! over FA2 is not available until vcores are dispatched on more than one core. What
//! *is* real and testable now is the structure - the staging is genuinely
//! double-buffered, the strands genuinely alternate at the fence, and the result is
//! asserted identical to FA2's. Building the pipeline before the parallelism is the
//! right order: the alternative is a pipeline whose first execution is also its first
//! test.
//!
//! ## Allocation
//!
//! Every function here takes its scratch as arguments (`s`, `acc`) rather than
//! allocating. A tile kernel's working set is a property of the tiling, so the caller
//! - which chose the tiling - is the only thing that knows how big it is; and this
//! module is included by the dependency-free build postures, where there is no
//! allocator to call.

use super::fmath::{expf, rowmax};

/// Why an attention call was refused.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AttnError {
    /// A slice is too short for the shape it was given.
    ShapeMismatch,
    /// `block_k` is zero, or a dimension is.
    BadTiling,
    /// The scratch buffers are too small for `block_k` / `d`.
    ScratchTooSmall,
}

/// Shape of one attention head.
#[derive(Copy, Clone, Debug)]
pub struct AttnShape {
    /// Query rows.
    pub tq: usize,
    /// Key/value rows (the context length).
    pub tk: usize,
    /// Head dimension.
    pub d: usize,
}

impl AttnShape {
    /// The `1/sqrt(d)` score scale, as an `f32`.
    ///
    /// Computed by Newton iteration rather than `sqrt`, which `core` does not provide
    /// for `f32` without `std`. Three iterations from a bit-trick seed converge to
    /// within an ulp for any `d` a head uses (64..256), and the caller may always
    /// pass its own scale to [`flash_attention_2`] instead.
    pub fn scale(&self) -> f32 {
        inv_sqrt(self.d as f32)
    }
}

/// `1/sqrt(x)` for positive `x`, by the classic exponent-halving seed plus Newton
/// refinement. `x <= 0` gives 0, which no caller reaches (a head dimension is
/// positive) and which is a defined answer rather than a NaN.
fn inv_sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    // Seed: negate and halve the exponent. The magic constant is the standard one;
    // it is a seed, and the iterations below are what make the result accurate, so
    // its provenance does not affect correctness.
    let mut y = f32::from_bits(0x5f37_59df - (x.to_bits() >> 1));
    // Newton on f(y) = 1/y^2 - x: y <- y * (1.5 - 0.5 * x * y^2).
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    y
}

/// **The naive reference**: materialise the whole `Tq x Tk` score matrix one row at a
/// time and softmax it.
///
/// Exists only as the oracle FlashAttention is checked against, and takes a full
/// `Tk`-long scratch row to make that explicit - the buffer FlashAttention is
/// designed never to need. Row-at-a-time rather than all at once so the reference
/// itself is usable at test sizes without a `Tq x Tk` allocation, which would make
/// the oracle the expensive part of the proof.
///
/// `q`, `k`, `v` are row-major `[tq, d]`, `[tk, d]`, `[tk, d]`; `o` is `[tq, d]`.
pub fn attention_reference(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    o: &mut [f32],
    shape: AttnShape,
    scale: f32,
    s: &mut [f32],
) -> Result<(), AttnError> {
    let AttnShape { tq, tk, d } = shape;
    check(q, k, v, o, shape)?;
    if s.len() < tk {
        return Err(AttnError::ScratchTooSmall);
    }
    for i in 0..tq {
        let qi = &q[i * d..i * d + d];
        for j in 0..tk {
            s[j] = dot(qi, &k[j * d..j * d + d]) * scale;
        }
        let m = rowmax(&s[..tk]);
        let mut l = 0.0f32;
        for j in 0..tk {
            s[j] = expf(s[j] - m);
            l += s[j];
        }
        let orow = &mut o[i * d..i * d + d];
        for x in orow.iter_mut() {
            *x = 0.0;
        }
        for j in 0..tk {
            let p = s[j];
            let vj = &v[j * d..j * d + d];
            for e in 0..d {
                orow[e] += p * vj[e];
            }
        }
        let inv = if l > 0.0 { 1.0 / l } else { 0.0 };
        for x in orow.iter_mut() {
            *x *= inv;
        }
    }
    Ok(())
}

/// **FlashAttention 2** forward for one head.
///
/// Walks `K`/`V` in blocks of `block_k` rows, keeping the running `(m, l, acc)` per
/// query row, so no `Tq x Tk` matrix exists at any point. `s` is scratch for one
/// block of scores (`>= block_k`) and `acc` for one output row (`>= d`).
///
/// The result is independent of `block_k` up to floating-point rounding; that is the
/// property [`super::attn`]'s proof asserts, and the one a bug in the rescale breaks.
pub fn flash_attention_2(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    o: &mut [f32],
    shape: AttnShape,
    scale: f32,
    block_k: usize,
    s: &mut [f32],
    acc: &mut [f32],
) -> Result<(), AttnError> {
    let AttnShape { tq, tk, d } = shape;
    check(q, k, v, o, shape)?;
    if block_k == 0 {
        return Err(AttnError::BadTiling);
    }
    if s.len() < block_k || acc.len() < d {
        return Err(AttnError::ScratchTooSmall);
    }
    for i in 0..tq {
        let qi = &q[i * d..i * d + d];
        let (m, l) = flash_row(qi, k, v, shape, scale, block_k, s, acc, 0, tk);
        let inv = if l > 0.0 { 1.0 / l } else { 0.0 };
        let orow = &mut o[i * d..i * d + d];
        for e in 0..d {
            orow[e] = acc[e] * inv;
        }
        // `m` is not part of the output - it is consumed by the normalisation - but
        // it is returned so a *split* accumulation (a caller that resumes over more
        // key blocks, which is how a paged-KV cache streams) can carry it forward.
        // Silently dropping it here would make that caller impossible to write
        // without reimplementing the loop.
        let _ = m;
    }
    Ok(())
}

/// One query row's online-softmax accumulation over key rows `[k_lo, k_hi)`.
///
/// Split out because it is the whole algorithm, and because it is what makes both
/// FA2 and the pipelined FA3 below the *same* arithmetic rather than two
/// implementations that have to be kept in agreement - the thing a bug hides
/// between. `acc` is **zeroed** here, so this is a fresh accumulation; a resuming
/// caller wants [`flash_row_resume`].
///
/// Returns the final `(m, l)`.
#[allow(clippy::too_many_arguments)]
fn flash_row(
    qi: &[f32],
    k: &[f32],
    v: &[f32],
    shape: AttnShape,
    scale: f32,
    block_k: usize,
    s: &mut [f32],
    acc: &mut [f32],
    k_lo: usize,
    k_hi: usize,
) -> (f32, f32) {
    for x in acc[..shape.d].iter_mut() {
        *x = 0.0;
    }
    flash_row_resume(
        qi,
        k,
        v,
        shape,
        scale,
        block_k,
        s,
        acc,
        k_lo,
        k_hi,
        f32::NEG_INFINITY,
        0.0,
    )
}

/// [`flash_row`] continuing from an existing `(m, l, acc)`.
///
/// This is the shape a **paged KV cache** needs: the key/value rows of one sequence
/// live in pages that are not contiguous, so a caller accumulates page by page and
/// must carry the running state across the gaps. It is also what makes the block
/// loop's correctness checkable in isolation - resuming with `(−inf, 0)` and a
/// zeroed `acc` must equal a fresh call, which is a property, not a coincidence.
#[allow(clippy::too_many_arguments)]
pub fn flash_row_resume(
    qi: &[f32],
    k: &[f32],
    v: &[f32],
    shape: AttnShape,
    scale: f32,
    block_k: usize,
    s: &mut [f32],
    acc: &mut [f32],
    k_lo: usize,
    k_hi: usize,
    m_in: f32,
    l_in: f32,
) -> (f32, f32) {
    let d = shape.d;
    let mut m = m_in;
    let mut l = l_in;
    let mut base = k_lo;
    while base < k_hi {
        let n = block_k.min(k_hi - base);
        // Scores for this block.
        for j in 0..n {
            let kj = &k[(base + j) * d..(base + j) * d + d];
            s[j] = dot(qi, kj) * scale;
        }
        let blk_max = rowmax(&s[..n]);
        let m_new = if blk_max > m { blk_max } else { m };
        // The correction that makes the running state comparable to the new max.
        // `m == -inf` on the first block would give `exp(-inf - m_new) = 0`, which is
        // right (there is nothing to carry) - but `-inf - -inf` is NaN if the block
        // is empty of finite scores, so the first-block case is taken explicitly
        // rather than relying on the arithmetic.
        let c = if m == f32::NEG_INFINITY {
            0.0
        } else if m_new == m {
            1.0
        } else {
            expf(m - m_new)
        };
        if c != 1.0 {
            l *= c;
            for x in acc[..d].iter_mut() {
                *x *= c;
            }
        }
        for j in 0..n {
            let p = expf(s[j] - m_new);
            l += p;
            let vj = &v[(base + j) * d..(base + j) * d + d];
            for e in 0..d {
                acc[e] += p * vj[e];
            }
        }
        m = m_new;
        base += n;
    }
    (m, l)
}

/// **FlashAttention 3**: FA2's arithmetic, pipelined over a double-buffered staging
/// pair.
///
/// The difference from [`flash_attention_2`] is *when* data moves, not what is
/// computed. Block `i+1`'s keys and values are staged into the idle half of a
/// double buffer while block `i` is being consumed from the other half, so the two
/// overlap instead of alternating. `stage` must hold `2 * block_k * d` elements for
/// keys and the same for values - the two halves.
///
/// `fence` is called at each buffer swap. That is the hook the strand-parallel
/// driver uses (`rt::yield_now`, so the producer strand runs), and it is a parameter
/// rather than a hard-coded yield because this function is also linked in postures
/// with no runtime at all, where the honest fence is a no-op.
///
/// The result is asserted identical to FA2's, which is the point: a pipeline that
/// changes the answer is not a pipeline, it is a different algorithm.
///
/// Honest: on one cooperative CPU the overlap is interleaving, not concurrency, so
/// this is structure ahead of the parallelism that pays for it (see the module docs).
#[allow(clippy::too_many_arguments)]
pub fn flash_attention_3(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    o: &mut [f32],
    shape: AttnShape,
    scale: f32,
    block_k: usize,
    s: &mut [f32],
    acc: &mut [f32],
    stage_k: &mut [f32],
    stage_v: &mut [f32],
    mut fence: impl FnMut(usize),
) -> Result<(), AttnError> {
    let AttnShape { tq, tk, d } = shape;
    check(q, k, v, o, shape)?;
    if block_k == 0 {
        return Err(AttnError::BadTiling);
    }
    if s.len() < block_k || acc.len() < d {
        return Err(AttnError::ScratchTooSmall);
    }
    let half = block_k * d;
    if stage_k.len() < 2 * half || stage_v.len() < 2 * half {
        return Err(AttnError::ScratchTooSmall);
    }

    for i in 0..tq {
        let qi = &q[i * d..i * d + d];
        for x in acc[..d].iter_mut() {
            *x = 0.0;
        }
        let mut m = f32::NEG_INFINITY;
        let mut l = 0.0f32;

        // Prologue: stage block 0 into buffer 0. The pipeline's steady state consumes
        // buffer `b` while staging into `b ^ 1`, so it needs one block already
        // resident before the loop - that prologue is the whole reason a pipelined
        // kernel is not just the same loop with a copy in it.
        let mut buf = 0usize;
        let mut staged = stage(k, v, stage_k, stage_v, buf, 0, block_k.min(tk), d);
        let mut base = 0usize;
        let mut blocks = 0usize;

        while base < tk {
            let n = staged;
            let next = base + n;
            // Stage the *next* block into the other half before consuming this one,
            // so the copy and the compute below are the two halves of the overlap.
            let next_n = if next < tk {
                let nn = block_k.min(tk - next);
                stage(k, v, stage_k, stage_v, buf ^ 1, next, nn, d);
                nn
            } else {
                0
            };
            fence(blocks);

            // Consume this half - the same arithmetic as `flash_row_resume`, over the
            // staged copy rather than the original arrays.
            let kb = &stage_k[buf * half..buf * half + n * d];
            let vb = &stage_v[buf * half..buf * half + n * d];
            for j in 0..n {
                s[j] = dot(qi, &kb[j * d..j * d + d]) * scale;
            }
            let blk_max = rowmax(&s[..n]);
            let m_new = if blk_max > m { blk_max } else { m };
            let c = if m == f32::NEG_INFINITY {
                0.0
            } else if m_new == m {
                1.0
            } else {
                expf(m - m_new)
            };
            if c != 1.0 {
                l *= c;
                for x in acc[..d].iter_mut() {
                    *x *= c;
                }
            }
            for j in 0..n {
                let p = expf(s[j] - m_new);
                l += p;
                let vj = &vb[j * d..j * d + d];
                for e in 0..d {
                    acc[e] += p * vj[e];
                }
            }
            m = m_new;

            base = next;
            staged = next_n;
            buf ^= 1;
            blocks += 1;
        }

        let inv = if l > 0.0 { 1.0 / l } else { 0.0 };
        let orow = &mut o[i * d..i * d + d];
        for e in 0..d {
            orow[e] = acc[e] * inv;
        }
    }
    Ok(())
}

/// Copy key/value rows `[row, row+n)` into half `buf` of the staging pair. Returns
/// `n`, so the caller's "how much is staged" and "how much did I stage" cannot drift.
fn stage(
    k: &[f32],
    v: &[f32],
    stage_k: &mut [f32],
    stage_v: &mut [f32],
    buf: usize,
    row: usize,
    n: usize,
    d: usize,
) -> usize {
    let half = stage_k.len() / 2;
    let off = buf * half;
    stage_k[off..off + n * d].copy_from_slice(&k[row * d..(row + n) * d]);
    let half_v = stage_v.len() / 2;
    let off_v = buf * half_v;
    stage_v[off_v..off_v + n * d].copy_from_slice(&v[row * d..(row + n) * d]);
    n
}

/// Elements a caller must provide for [`flash_attention_2`]'s scratch, and for
/// [`flash_attention_3`]'s staging, given a tiling.
///
/// A function rather than arithmetic at each call site: getting the staging size
/// wrong is an `Err(ScratchTooSmall)` at best and a wrong answer if the check were
/// ever relaxed, and "two halves of block_k rows of d" is exactly the sort of
/// expression that gets one factor dropped.
pub const fn fa3_stage_len(block_k: usize, d: usize) -> usize {
    2 * block_k * d
}

fn check(q: &[f32], k: &[f32], v: &[f32], o: &[f32], shape: AttnShape) -> Result<(), AttnError> {
    let AttnShape { tq, tk, d } = shape;
    if tq == 0 || tk == 0 || d == 0 {
        return Err(AttnError::BadTiling);
    }
    if q.len() < tq * d || k.len() < tk * d || v.len() < tk * d || o.len() < tq * d {
        return Err(AttnError::ShapeMismatch);
    }
    Ok(())
}

/// Dot product of two equal-length rows. Scalar; `tile::simd` is where a
/// target-specific kernel would go, and the GEMM there is the precedent - the point
/// of keeping this scalar is that it is the oracle the vector path is checked
/// against.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut s = 0.0f32;
    for e in 0..a.len().min(b.len()) {
        s += a[e] * b[e];
    }
    s
}

/// Largest relative difference between two equal-length buffers, with an absolute
/// floor so a near-zero pair is not judged by a ratio of noise.
///
/// The comparison FlashAttention has to be judged by. Bit equality is the wrong
/// test - the summation order genuinely differs - and a bare absolute difference
/// would pass or fail depending on the magnitude of the values, which is a property
/// of the test data rather than of the kernel.
pub fn max_rel_diff(a: &[f32], b: &[f32]) -> f32 {
    let mut worst = 0.0f32;
    for i in 0..a.len().min(b.len()) {
        let (x, y) = (a[i], b[i]);
        let diff = if x > y { x - y } else { y - x };
        let mag = {
            let ax = if x < 0.0 { -x } else { x };
            let ay = if y < 0.0 { -y } else { y };
            let m = if ax > ay { ax } else { ay };
            // The floor: below it, judge absolutely. 1e-6 is well above f32 noise for
            // values of order 1, which softmax outputs are (a convex combination of
            // the V rows).
            if m > 1e-6 { m } else { 1e-6 }
        };
        let rel = diff / mag;
        if rel > worst {
            worst = rel;
        }
    }
    worst
}
