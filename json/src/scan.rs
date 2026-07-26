//! Scanning a JSON string body for the next byte that needs handling: a quote
//! (end of string), a backslash (escape), or a control byte (< 0x20, illegal
//! unescaped). This is the parser's hottest inner loop on string-heavy input.
//!
//! The scalar version runs everywhere - including in a cell. A SIMD version
//! (SSE2, 16 bytes/step) accelerates the host benchmark under `feature =
//! "simd"`. This is the OS's "measured runtime dispatch over wide SIMD"
//! (ARCHITECTURE.md 1.4) at the build level for now; a true per-cell runtime
//! dispatch waits on U-mode vector-state enablement, so the on-OS build stays
//! scalar. Both paths are proven identical by the fuzz test below.

/// Offset within `buf` of the first byte that is `"`, `\`, or `< 0x20`, or
/// `buf.len()` if none. Uses SIMD when built with `feature = "simd"` on x86-64.
#[inline]
pub fn string_event(buf: &[u8]) -> usize {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        // SAFETY: SSE2 is baseline on every x86-64 CPU.
        return unsafe { string_event_sse2(buf) };
    }
    #[allow(unreachable_code)]
    string_event_scalar(buf)
}

pub fn string_event_scalar(buf: &[u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            return i;
        }
        i += 1;
    }
    buf.len()
}

#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn string_event_sse2(buf: &[u8]) -> usize {
    use core::arch::x86_64::*;
    let n = buf.len();
    let quote = _mm_set1_epi8(b'"' as i8);
    let backslash = _mm_set1_epi8(b'\\' as i8);
    let ctrl = _mm_set1_epi8(0x1f);
    let mut i = 0;
    // SAFETY: reads stay within [buf, buf+n) - the 16-byte loads only run
    // while i + 16 <= n, and the tail is handled byte-by-byte.
    unsafe {
        while i + 16 <= n {
            let v = _mm_loadu_si128(buf.as_ptr().add(i) as *const __m128i);
            let eq_q = _mm_cmpeq_epi8(v, quote);
            let eq_b = _mm_cmpeq_epi8(v, backslash);
            // control byte: b <= 0x1f  <=>  min_epu8(b, 0x1f) == b
            let is_ctrl = _mm_cmpeq_epi8(_mm_min_epu8(v, ctrl), v);
            let hit = _mm_or_si128(_mm_or_si128(eq_q, eq_b), is_ctrl);
            let mask = _mm_movemask_epi8(hit) as u32;
            if mask != 0 {
                return i + mask.trailing_zeros() as usize;
            }
            i += 16;
        }
    }
    while i < n {
        let b = buf[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            return i;
        }
        i += 1;
    }
    n
}

#[cfg(all(test, feature = "simd", target_arch = "x86_64"))]
mod fuzz {
    use super::*;

    // A cheap deterministic PRNG so the fuzz is reproducible without deps.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    #[test]
    fn simd_matches_scalar() {
        let mut st = 0x1234_5678u64;
        // Bytes biased toward the interesting set so hits land at every offset.
        let alphabet = b"ab \"\\\x01\x1f{}c\n";
        for _ in 0..20_000 {
            let len = (lcg(&mut st) % 64) as usize;
            let mut buf = alloc::vec::Vec::with_capacity(len);
            for _ in 0..len {
                buf.push(alphabet[(lcg(&mut st) as usize) % alphabet.len()]);
            }
            assert_eq!(
                string_event(&buf),
                string_event_scalar(&buf),
                "mismatch on {buf:?}"
            );
        }
    }
}
