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
                let mut qi = r as i32;
                if qi > 127 {
                    qi = 127;
                }
                if qi < -127 {
                    qi = -127;
                }
                *dst.add(i) = qi as i8;
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
            let mut v = *src.add(i) >> shift;
            if v > 127 {
                v = 127;
            }
            if v < -127 {
                v = -127;
            }
            *dst.add(i) = v as i8;
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
