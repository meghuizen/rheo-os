// Host entropy benchmark (docs/TIME-IDENTITY.md 4): measures the entropy
// *sources and pool*, where getrandom_bench.rs measures the DRBG *draw
// path*. Three questions, answered on real hardware:
//
// 1. **Jitter quality/rate.** The kernel credits timing jitter at 1/4 bit
//    per noisy delta (the same estimator as kernel/src/rng/pool.rs,
//    mirrored below). How many credited bits per second does a real CPU
//    produce, and how long does reaching the 256-bit seeding gate take on
//    jitter alone? (In QEMU under -icount the honest answer is "never" -
//    the estimator credits ~0 there, which is exactly the design.)
// 2. **Estimator sanity.** Constant input must credit 0; the credit must
//    never exceed the 1/4-bit-per-sample bound.
// 3. **Acceleration headroom.** The kernel ChaCha20 is scalar (the kernel
//    targets are soft-float: no SIMD without U-mode FP/SIMD state work).
//    Where AVX2 exists, an 8-block interleaved path shows what runtime
//    dispatch buys - checked bit-for-bit against the scalar core first.
//
// Build + run: comparison/rng/run.sh  (plain rustc, no crates).

#![allow(clippy::needless_range_loop)]

use std::time::Instant;

// The exact kernel ChaCha20 block. include! keeps it byte-identical.
include!("../../kernel/src/rng/chacha.rs");

// ---- the kernel's jitter estimator, mirrored ---------------------------
// (kernel/src/rng/pool.rs::estimate_jitter_bits - delta-of-delta, low
// nibble neither 0x0 nor 0xF, 1/4 bit per noisy sample, branchless.)

#[inline]
fn ct_eq64(a: u64, b: u64) -> u64 {
    let x = a ^ b;
    1 ^ ((x | x.wrapping_neg()) >> 63)
}

fn estimate_jitter_bits(deltas: &[u64]) -> u32 {
    if deltas.len() < 2 {
        return 0;
    }
    let mut noisy = 0u64;
    for i in 1..deltas.len() {
        let d2 = deltas[i].wrapping_sub(deltas[i - 1]);
        let low = d2 & 0xF;
        noisy += (1 ^ ct_eq64(low, 0x0)) & (1 ^ ct_eq64(low, 0xF));
    }
    (noisy / 4) as u32
}

#[cfg(target_arch = "x86_64")]
fn cycles() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}
#[cfg(not(target_arch = "x86_64"))]
fn cycles() -> u64 {
    Instant::now().elapsed().as_nanos() as u64 // placeholder off-x86 hosts
}

/// One jitter window, kernel-shaped: dependent ALU work + a table touch
/// between cycle-counter reads.
fn jitter_window(salt: u64) -> [u64; 64] {
    let mut deltas = [0u64; 64];
    let mut table = [0u64; 16];
    let mut acc = salt | 1;
    let mut prev = cycles();
    for i in 0..64 {
        acc = acc.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i as u64);
        acc ^= acc >> 29;
        let idx = (acc & 0xF) as usize;
        table[idx] = table[idx].wrapping_add(acc);
        let now = cycles();
        deltas[i] = now.wrapping_sub(prev);
        prev = now;
    }
    std::hint::black_box(&table);
    deltas
}

// ---- AVX2 8-block ChaCha20 (x86-64 only, runtime-dispatched) ------------

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    /// Eight ChaCha20 blocks at once: each __m256i holds one state word
    /// across 8 blocks (epi32 lane i = block i, counters c..c+7).
    /// Rotations by shift-or; the same ARX network as the scalar core.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available (is_x86_feature_detected).
    #[target_feature(enable = "avx2")]
    pub unsafe fn block8(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 512]) {
        unsafe {
            let le = |b: &[u8], i: usize| {
                u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as i32
            };
            let mut init = [0i32; 16];
            init[0] = 0x6170_7865u32 as i32;
            init[1] = 0x3320_646eu32 as i32;
            init[2] = 0x7962_2d32u32 as i32;
            init[3] = 0x6b20_6574u32 as i32;
            for w in 0..8 {
                init[4 + w] = le(key, w * 4);
            }
            init[12] = counter as i32;
            init[13] = le(nonce, 0);
            init[14] = le(nonce, 4);
            init[15] = le(nonce, 8);

            let mut x: [__m256i; 16] = [_mm256_setzero_si256(); 16];
            let mut orig: [__m256i; 16] = [_mm256_setzero_si256(); 16];
            for w in 0..16 {
                orig[w] = _mm256_set1_epi32(init[w]);
            }
            // Per-block counters: lane i runs block counter + i.
            orig[12] = _mm256_add_epi32(
                _mm256_set1_epi32(counter as i32),
                _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7),
            );
            x.copy_from_slice(&orig);

            macro_rules! rotl {
                ($v:expr, $n:literal) => {
                    _mm256_or_si256(
                        _mm256_slli_epi32::<$n>($v),
                        _mm256_srli_epi32::<{ 32 - $n }>($v),
                    )
                };
            }
            macro_rules! qr {
                ($a:literal, $b:literal, $c:literal, $d:literal) => {
                    x[$a] = _mm256_add_epi32(x[$a], x[$b]);
                    x[$d] = rotl!(_mm256_xor_si256(x[$d], x[$a]), 16);
                    x[$c] = _mm256_add_epi32(x[$c], x[$d]);
                    x[$b] = rotl!(_mm256_xor_si256(x[$b], x[$c]), 12);
                    x[$a] = _mm256_add_epi32(x[$a], x[$b]);
                    x[$d] = rotl!(_mm256_xor_si256(x[$d], x[$a]), 8);
                    x[$c] = _mm256_add_epi32(x[$c], x[$d]);
                    x[$b] = rotl!(_mm256_xor_si256(x[$b], x[$c]), 7);
                };
            }
            for _ in 0..10 {
                qr!(0, 4, 8, 12);
                qr!(1, 5, 9, 13);
                qr!(2, 6, 10, 14);
                qr!(3, 7, 11, 15);
                qr!(0, 5, 10, 15);
                qr!(1, 6, 11, 12);
                qr!(2, 7, 8, 13);
                qr!(3, 4, 9, 14);
            }
            for w in 0..16 {
                x[w] = _mm256_add_epi32(x[w], orig[w]);
            }
            // Serialise: block i's word w sits in epi32 lane i of x[w].
            let mut lanes = [0i32; 8];
            for w in 0..16 {
                _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, x[w]);
                for blk in 0..8 {
                    let v = lanes[blk] as u32;
                    let o = blk * 64 + w * 4;
                    out[o..o + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
}

fn main() {
    println!("== entropy source + acceleration benchmark (host) ==\n");

    // ---- 1. estimator sanity -------------------------------------------
    assert_eq!(estimate_jitter_bits(&[42u64; 64]), 0);
    let win = jitter_window(cycles());
    let bits = estimate_jitter_bits(&win);
    assert!(bits <= 16, "estimator exceeded the 1/4-bit bound");
    println!("estimator sanity: constant=0 bits, live window={bits} bits (cap 16) .. OK");

    // ---- 2. jitter quality and rate ------------------------------------
    let rounds = 4096;
    let start = Instant::now();
    let mut credited = 0u64;
    let mut zero_windows = 0u32;
    for r in 0..rounds {
        let w = jitter_window(r as u64);
        let b = estimate_jitter_bits(&w) as u64;
        credited += b;
        if b == 0 {
            zero_windows += 1;
        }
    }
    let el = start.elapsed();
    let bits_per_sec = credited as f64 / el.as_secs_f64();
    let to_gate_us = if credited > 0 {
        256.0 / bits_per_sec * 1e6
    } else {
        f64::INFINITY
    };
    println!(
        "jitter: {credited} credited bits over {rounds} windows ({} samples) in {:?}",
        rounds * 64,
        el
    );
    println!(
        "jitter: {:.0} credited bits/s -> 256-bit seeding gate in ~{:.1} us on this host \
         ({zero_windows} windows credited 0)",
        bits_per_sec, to_gate_us
    );
    println!(
        "jitter: credit density {:.3} bits/sample (bound 0.25) - conservative under-crediting\n",
        credited as f64 / (rounds * 64) as f64
    );

    // ---- 3. scalar vs AVX2 throughput -----------------------------------
    let key = [0x42u8; 32];
    let nonce = [7u8; 12];
    const TOTAL: usize = 64 * 1024 * 1024;

    let mut sink = 0u8;
    let start = Instant::now();
    let mut blk = [0u8; 64];
    for i in 0..(TOTAL / 64) {
        block(&key, i as u32, &nonce, &mut blk);
        sink ^= blk[0];
    }
    let scalar = start.elapsed();
    println!(
        "chacha20 scalar (the kernel core):   {:>7.1} MB/s",
        TOTAL as f64 / scalar.as_secs_f64() / 1e6
    );

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        // Verify the AVX2 path bit-for-bit against the scalar core first.
        let mut eight = [0u8; 512];
        unsafe { avx2::block8(&key, 100, &nonce, &mut eight) };
        for b in 0..8 {
            let mut want = [0u8; 64];
            block(&key, 100 + b as u32, &nonce, &mut want);
            assert!(
                eight[b * 64..(b + 1) * 64] == want,
                "AVX2 block {} mismatch vs scalar core",
                b
            );
        }
        println!("chacha20 avx2 x8: verified bit-for-bit against the scalar core");

        let start = Instant::now();
        for i in 0..(TOTAL / 512) {
            unsafe { avx2::block8(&key, (i * 8) as u32, &nonce, &mut eight) };
            sink ^= eight[0];
        }
        let simd = start.elapsed();
        println!(
            "chacha20 avx2 x8 (runtime-dispatched): {:>5.1} MB/s ({:.2}x scalar)",
            TOTAL as f64 / simd.as_secs_f64() / 1e6,
            scalar.as_secs_f64() / simd.as_secs_f64()
        );
        println!(
            "note: the kernel cannot take this path yet - its targets are soft-float\n\
             (no SIMD state save on traps); this measures the headroom runtime\n\
             dispatch will buy when U-mode FP/SIMD state handling lands."
        );
    } else {
        println!("no AVX2 on this host - scalar only");
    }
    std::hint::black_box(sink);
}
