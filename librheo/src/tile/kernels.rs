// Canonical scalar tile kernels (docs/TILES.md). This file is DEPENDENCY-FREE
// on purpose - no `use`, no crate imports, no_std-safe - because it is shared
// VERBATIM by four consumers: the librheo CpuExecutor (`mod kernels` below
// tile/mod.rs), the kernel engine (`#[path]` source include - not a cargo
// dependency, so the kernel's zero-deps rule holds), the bench-core `p5_*`
// benches, and the `comparison/tiles` host benchmark (the include-the-shipped-
// code rule from comparison/threads). Regular `//` comments keep it includable.
//
// The kernels are LOOP BODIES ONLY: the tiled loop around them is executor
// logic (async + yield in librheo, a bounded synchronous loop in the kernel
// engine, a plain loop on the host). Integer paths are exact on every ISA;
// there is no float kernel here (the kernel engine is integer-only - the
// aarch64 kernel target is soft-float; docs/TILES.md 6).

/// Accumulating int8 GEMM: `C[m,n] += A[m,k] * B[k,n]`, i32 accumulate.
/// Row-major with element strides `as_`/`bs`/`cs` (elements, not bytes).
/// The caller zeroes C for a fresh product; block-tiled callers invoke this
/// per (m,k)x(k,n) block so k-blocks accumulate correctly.
///
/// # Safety
/// `a`, `b`, `c` must be valid for the strided `m x k`, `k x n`, `m x n`
/// accesses; C must not alias A or B.
#[allow(clippy::too_many_arguments)] // a GEMM is (A,lda, B,ldb, C,ldc, m,n,k)
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
    unsafe {
        for i in 0..m {
            let arow = a.add(i * as_);
            let crow = c.add(i * cs);
            for p in 0..k {
                let av = *arow.add(p) as i32;
                if av == 0 {
                    continue;
                }
                let brow = b.add(p * bs);
                for j in 0..n {
                    *crow.add(j) += av * (*brow.add(j) as i32);
                }
            }
        }
    }
}

/// Wrapping u64 sum over `elems` elements of `dtype` (docs/TILES.md dtype
/// codes: 0=I8, 1=U8, 2=I32). Signed dtypes sign-extend before the wrapping
/// add, so the receipt is deterministic and ISA-independent. Returns 0 for a
/// dtype outside the reducible set (callers validate first).
///
/// # Safety
/// `ptr` must be valid for `elems` elements of the given dtype.
pub unsafe fn reduce_wrapping(ptr: *const u8, elems: usize, dtype: u32) -> u64 {
    let mut acc = 0u64;
    unsafe {
        match dtype {
            0 => {
                let p = ptr as *const i8;
                for i in 0..elems {
                    acc = acc.wrapping_add(*p.add(i) as i64 as u64);
                }
            }
            1 => {
                for i in 0..elems {
                    acc = acc.wrapping_add(*ptr.add(i) as u64);
                }
            }
            2 => {
                let p = ptr as *const i32;
                for i in 0..elems {
                    acc = acc.wrapping_add(*p.add(i) as i64 as u64);
                }
            }
            _ => {}
        }
    }
    acc
}

/// Per-block symmetric quantization f32 -> i8: for each block of `block`
/// elements (the last block may be a tail), scale = max|x| / 127 (1.0 when
/// the block is all zero), `dst = round(src / scale)` clamped to [-127, 127].
/// One f32 scale per block is written to `scales`.
///
/// # Safety
/// `src` valid for `elems` f32; `dst` for `elems` i8; `scales` for
/// `ceil(elems / block)` f32. `block` must be non-zero.
pub unsafe fn quant_f32_i8(
    src: *const f32,
    dst: *mut i8,
    scales: *mut f32,
    elems: usize,
    block: usize,
) {
    unsafe {
        let nblocks = elems.div_ceil(block);
        for bi in 0..nblocks {
            let lo = bi * block;
            let hi = if lo + block < elems {
                lo + block
            } else {
                elems
            };
            let mut maxabs = 0.0f32;
            for i in lo..hi {
                let v = *src.add(i);
                let a = if v < 0.0 { -v } else { v };
                if a > maxabs {
                    maxabs = a;
                }
            }
            let scale = if maxabs == 0.0 { 1.0 } else { maxabs / 127.0 };
            *scales.add(bi) = scale;
            for i in lo..hi {
                let q = *src.add(i) / scale;
                let r = if q >= 0.0 { q + 0.5 } else { q - 0.5 };
                *dst.add(i) = (r as i32).clamp(-127, 127) as i8;
            }
        }
    }
}

/// Per-block dequantization i8 -> f32 with the scale plane from
/// [`quant_f32_i8`].
///
/// # Safety
/// `src` valid for `elems` i8; `scales` for `ceil(elems / block)` f32; `dst`
/// for `elems` f32. `block` must be non-zero.
pub unsafe fn dequant_i8_f32(
    src: *const i8,
    scales: *const f32,
    dst: *mut f32,
    elems: usize,
    block: usize,
) {
    unsafe {
        for i in 0..elems {
            let scale = *scales.add(i / block);
            *dst.add(i) = (*src.add(i) as f32) * scale;
        }
    }
}

/// Integer requantize: `dst = clamp(src >> shift, -127, 127)` (arithmetic
/// shift). The attention pipeline's integer softmax-slot map (docs/TILES.md
/// 10): scores come out of a GEMM as i32 and re-enter the next GEMM as i8.
///
/// # Safety
/// `src` valid for `elems` i32; `dst` for `elems` i8.
pub unsafe fn shift_clamp_i32_i8(src: *const i32, dst: *mut i8, elems: usize, shift: u32) {
    unsafe {
        for i in 0..elems {
            *dst.add(i) = (*src.add(i) >> shift).clamp(-127, 127) as i8;
        }
    }
}

/// FNV-1a over a byte slice - the deterministic receipt hash for tile-op
/// results (docs/TILES.md 6). Stable across ISAs and executors.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Narrow-float and sub-byte conversions (docs/TILES.md 2). Pure bit math -
// deterministic, ISA-independent, soft-float-safe (the only f32 arithmetic is
// in the block-quant scale paths). Rounding is round-to-nearest-even via the
// standard "add half-ulp of the kept mantissa + tie fix" trick. Each format
// is the storage-conversion half of its dtype; MMA over these formats is a
// device-engine lowering (declared, not run here).
// ---------------------------------------------------------------------------

/// f32 -> IEEE binary16 bits (round-to-nearest-even, overflow -> inf,
/// preserves NaN/inf/signed zero; denormals flush through the standard path).
pub fn f32_to_f16_bits(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let man = bits & 0x007F_FFFF;
    if exp == 0xFF {
        // Inf / NaN (keep a NaN payload bit so NaN stays NaN).
        return sign | 0x7C00 | if man != 0 { 0x0200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1F {
        return sign | 0x7C00; // overflow -> inf
    }
    if e <= 0 {
        // Subnormal half (or zero): shift the implicit-1 mantissa down.
        if e < -10 {
            return sign;
        }
        let m = man | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let mut v = ((m + half) >> shift) as u16;
        // Tie-to-even fix: a tie that rounded up to an odd value goes down.
        if (m & ((1 << shift) - 1)) == half && (v & 1) == 1 {
            v -= 1;
        }
        return sign | v;
    }
    // Normal: round the 23-bit mantissa to 10 bits (RNE).
    let mut m = man >> 13;
    let rem = man & 0x1FFF;
    if rem > 0x1000 || (rem == 0x1000 && (m & 1) == 1) {
        m += 1;
    }
    let mut ee = e as u32;
    if m == 0x400 {
        m = 0;
        ee += 1;
        if ee >= 0x1F {
            return sign | 0x7C00;
        }
    }
    sign | ((ee as u16) << 10) | (m as u16)
}

/// IEEE binary16 bits -> f32 (exact).
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1F) as u32;
    let man = (h & 0x3FF) as u32;
    let bits = if exp == 0 {
        if man == 0 {
            sign
        } else {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((m & 0x3FF) << 13)
        }
    } else if exp == 0x1F {
        sign | 0x7F80_0000 | (man << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (man << 13)
    };
    f32::from_bits(bits)
}

/// f32 -> bfloat16 bits (round-to-nearest-even; bf16 is the top 16 bits of
/// f32, so this is the canonical truncate-with-rounding).
pub fn f32_to_bf16_bits(x: f32) -> u16 {
    let bits = x.to_bits();
    if x.is_nan() {
        return ((bits >> 16) as u16) | 0x0040; // keep NaN quiet
    }
    let round = 0x7FFF + ((bits >> 16) & 1);
    ((bits + round) >> 16) as u16
}

/// bfloat16 bits -> f32 (exact: shift back up).
pub fn bf16_bits_to_f32(h: u16) -> f32 {
    f32::from_bits((h as u32) << 16)
}

/// f32 -> TF32 (the 19-bit tensor-float: f32 range, 10-bit mantissa),
/// stored in an f32 slot with the low 13 mantissa bits zero (the storage
/// convention; "bfloat32" in some vendors' vocabulary). RNE on the kept bits.
pub fn f32_to_tf32(x: f32) -> f32 {
    let bits = x.to_bits();
    if x.is_nan() {
        return x;
    }
    let round = 0x0FFF + ((bits >> 13) & 1);
    f32::from_bits((bits + round) & !0x1FFF)
}

/// f32 -> FP8 E4M3 bits (the OCP/NV format: bias 7, saturating to +-448,
/// no inf - overflow saturates to the max finite value; NaN -> 0x7F).
pub fn f32_to_f8e4m3_bits(x: f32) -> u8 {
    let bits = x.to_bits();
    let sign = ((bits >> 24) & 0x80) as u8;
    if x.is_nan() {
        return sign | 0x7F;
    }
    let a = if x < 0.0 { -x } else { x };
    if a >= 448.0 {
        return sign | 0x7E; // saturate to max finite (E4M3 has no inf)
    }
    if a < 0.001953125 {
        // Below the smallest subnormal/2 (2^-9): round the subnormal range
        // in units of 2^-9 (the E4M3 subnormal step).
        let q = a / 0.001953125;
        let r = (q + 0.5) as u32; // ties handled coarsely below the normal range
        if r == 0 {
            return sign;
        }
        if r < 8 {
            return sign | (r as u8);
        }
    }
    // Normal path: exponent bias 7, 3 mantissa bits, RNE.
    let exp = (((bits >> 23) & 0xFF) as i32) - 127;
    let man = bits & 0x007F_FFFF;
    let mut e = exp + 7;
    let mut m = man >> 20;
    let rem = man & 0x000F_FFFF;
    if rem > 0x8_0000 || (rem == 0x8_0000 && (m & 1) == 1) {
        m += 1;
    }
    if m == 8 {
        m = 0;
        e += 1;
    }
    if e <= 0 {
        // Subnormal: value = m * 2^-9 with the implicit 1 folded in.
        let q = a / 0.001953125;
        let r = (q + 0.5) as u32;
        return sign | (r.min(7) as u8);
    }
    if e >= 0x10 || (e == 0xF && m == 7) {
        return sign | 0x7E; // saturate (0x7F is NaN in E4M3)
    }
    sign | ((e as u8) << 3) | (m as u8)
}

/// FP8 E4M3 bits -> f32 (exact).
pub fn f8e4m3_bits_to_f32(b: u8) -> f32 {
    let sign = if b & 0x80 != 0 { -1.0f32 } else { 1.0 };
    let e = ((b >> 3) & 0xF) as i32;
    let m = (b & 0x7) as f32;
    if e == 0xF && (b & 0x7) == 0x7 {
        return f32::NAN * sign;
    }
    if e == 0 {
        return sign * m * 0.001953125; // subnormal: m * 2^-9
    }
    sign * (1.0 + m / 8.0) * pow2f(e - 7)
}

/// f32 -> FP8 E5M2 bits (the "bfloat8" truncation format: bias 15, 2
/// mantissa bits, has inf). RNE; overflow -> inf.
pub fn f32_to_f8e5m2_bits(x: f32) -> u8 {
    // E5M2 is f16 truncated to its top byte - reuse the f16 path with RNE
    // on the dropped 8 bits.
    let h = f32_to_f16_bits(x);
    let round = 0x7F + ((h >> 8) & 1);
    let r = (h as u32) + round as u32;
    // Overflow past f16 inf stays inf-of-e5m2 (top byte 0x7C..).
    (r >> 8) as u8
}

/// FP8 E5M2 bits -> f32 (exact: widen through f16).
pub fn f8e5m2_bits_to_f32(b: u8) -> f32 {
    f16_bits_to_f32((b as u16) << 8)
}

/// Per-block symmetric int4 quantization (block-quant, docs/TILES.md 2):
/// scale = max|x| / 7 per `block` elements; codes are signed 4-bit two's
/// complement packed two per byte (low nibble first). `dst` holds
/// ceil(elems/2) bytes; `scales` one f32 per block.
///
/// # Safety
/// `src` valid for `elems` f32; `dst` for `ceil(elems/2)` bytes; `scales`
/// for `ceil(elems/block)` f32. `block` must be non-zero.
pub unsafe fn quant_f32_i4b(
    src: *const f32,
    dst: *mut u8,
    scales: *mut f32,
    elems: usize,
    block: usize,
) {
    unsafe {
        let nblocks = elems.div_ceil(block);
        for bi in 0..nblocks {
            let lo = bi * block;
            let hi = if lo + block < elems {
                lo + block
            } else {
                elems
            };
            let mut maxabs = 0.0f32;
            for i in lo..hi {
                let v = *src.add(i);
                let a = if v < 0.0 { -v } else { v };
                if a > maxabs {
                    maxabs = a;
                }
            }
            let scale = if maxabs == 0.0 { 1.0 } else { maxabs / 7.0 };
            *scales.add(bi) = scale;
            for i in lo..hi {
                let q = *src.add(i) / scale;
                let r = if q >= 0.0 { q + 0.5 } else { q - 0.5 };
                let nib = ((r as i32).clamp(-7, 7) as u8) & 0x0F;
                let byte = dst.add(i / 2);
                if i % 2 == 0 {
                    *byte = (*byte & 0xF0) | nib;
                } else {
                    *byte = (*byte & 0x0F) | (nib << 4);
                }
            }
        }
    }
}

/// Per-block int4 dequantization (the inverse of [`quant_f32_i4b`]).
///
/// # Safety
/// As for `quant_f32_i4b`, with `dst` valid for `elems` f32.
pub unsafe fn dequant_i4b_f32(
    src: *const u8,
    scales: *const f32,
    dst: *mut f32,
    elems: usize,
    block: usize,
) {
    unsafe {
        for i in 0..elems {
            let byte = *src.add(i / 2);
            let nib = if i % 2 == 0 { byte & 0x0F } else { byte >> 4 };
            // Sign-extend the 4-bit two's complement code.
            let q = ((nib << 4) as i8) >> 4;
            *dst.add(i) = (q as f32) * *scales.add(i / block);
        }
    }
}

/// 2^e as f32 for small exponents (no libm in no_std).
fn pow2f(e: i32) -> f32 {
    if e >= 0 {
        (1u64 << e.min(63)) as f32
    } else {
        1.0 / ((1u64 << (-e).min(63)) as f32)
    }
}
