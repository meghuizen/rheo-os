//! Byte scanning for the HTTP/1.1 parser - the hottest inner loop on
//! header-heavy input (docs/NETSTACK.md §19). This reuses the **shape** of
//! `json/src/scan.rs`: a plain scalar version that is the oracle, plus a
//! branchless wide version that processes 8 bytes per step, and a fuzz
//! equivalence check proving the two agree.
//!
//! ## Why SWAR and not SSE2 (the portability rule)
//! `json/src/scan.rs` accelerates with SSE2 behind `cfg(target_arch =
//! "x86_64")`. rheo-net must not carry per-ISA code (docs/TARGET-ARCHITECTURES.md
//! 4 - only `kernel/src/arch/` may differ per ISA), so the wide path here is
//! **SWAR**: the same load / compare / mask / count-trailing-zeros pipeline, but
//! expressed in `u64` integer arithmetic, which the compiler lowers to whatever
//! the target has. It is branchless per 8-byte word, portable to all three ISAs
//! with no `cfg`, and needs no runtime feature detection. A target-specific SIMD
//! kernel (SSE2/NEON/RVV) behind a measured dispatch is deferred - the SWAR path
//! is the portable floor every ISA gets.
//!
//! ## The bit trick (documented, because it is not obvious)
//! For a 64-bit word `w` holding 8 bytes and a target byte `c`:
//!
//! ```text
//!   x    = w ^ (c repeated 8 times)     // zero byte exactly where w == c
//!   hits = (x - 0x0101..01) & !x & 0x8080..80
//! ```
//!
//! `hits` has the high bit of a byte lane set iff that lane of `x` was zero,
//! i.e. iff that byte of `w` equalled `c`. The word is loaded with
//! `u64::from_le_bytes`, so lane 0 is always the *first* byte regardless of host
//! endianness, and `hits.trailing_zeros() / 8` is the index of the first match.

/// The low bit of every byte lane.
const LO: u64 = 0x0101_0101_0101_0101;
/// The high bit of every byte lane.
const HI: u64 = 0x8080_8080_8080_8080;

/// Broadcast `c` into all eight byte lanes.
#[inline]
const fn splat(c: u8) -> u64 {
    (c as u64) * LO
}

/// Byte lanes of `x` that are zero, marked in their high bit.
#[inline]
const fn zero_lanes(x: u64) -> u64 {
    x.wrapping_sub(LO) & !x & HI
}

/// Offset within `buf` of the first byte equal to `needle`, or `buf.len()` if
/// there is none. The **oracle**: a plain byte loop, always correct.
pub fn find_byte_scalar(buf: &[u8], needle: u8) -> usize {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == needle {
            return i;
        }
        i += 1;
    }
    buf.len()
}

/// Offset within `buf` of the first byte equal to `needle`, or `buf.len()`.
/// Branchless 8 bytes per step (SWAR), with a scalar tail. Proven identical to
/// [`find_byte_scalar`] by the fuzz-equivalence check the `nethttp` proof runs.
pub fn find_byte(buf: &[u8], needle: u8) -> usize {
    let n = buf.len();
    let target = splat(needle);
    let mut i = 0;
    while i + 8 <= n {
        // `from_le_bytes` pins lane 0 to the first byte on every ISA.
        let w = u64::from_le_bytes([
            buf[i],
            buf[i + 1],
            buf[i + 2],
            buf[i + 3],
            buf[i + 4],
            buf[i + 5],
            buf[i + 6],
            buf[i + 7],
        ]);
        let hits = zero_lanes(w ^ target);
        if hits != 0 {
            return i + (hits.trailing_zeros() as usize) / 8;
        }
        i += 8;
    }
    while i < n {
        if buf[i] == needle {
            return i;
        }
        i += 1;
    }
    n
}

/// Offset of the first byte that is **not** an RFC 9110 `tchar` (a valid header
/// name / method character), or `buf.len()` if every byte is a tchar. Table
/// driven, one branch per byte - header names are short, so the table lookup is
/// already the cheap path and a wide version would not pay.
pub fn token_end(buf: &[u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        if !is_tchar(buf[i]) {
            return i;
        }
        i += 1;
    }
    buf.len()
}

/// RFC 9110 §5.6.2 `tchar`: `!#$%&'*+-.^_`|~`, DIGIT, ALPHA. Everything else -
/// notably space, HTAB, colon, and any control or 8-bit byte - is **not** a
/// token character, which is what makes a non-token header name detectable.
pub const fn is_tchar(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
        | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

/// True if `b` is a legal header **field-value** byte: visible ASCII, SP, HTAB,
/// or an obs-text byte (0x80-0xFF). Control bytes - in particular a stray CR, LF
/// or NUL, the header-injection vectors - are rejected.
pub const fn is_field_vchar(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b <= 0x7e) || b >= 0x80
}

/// Strip leading and trailing SP / HTAB (RFC 9110 `OWS`) from a field value.
pub fn trim_ows(mut v: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = v {
        if *first == b' ' || *first == b'\t' {
            v = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = v {
        if *last == b' ' || *last == b'\t' {
            v = rest;
        } else {
            break;
        }
    }
    v
}

/// ASCII case-insensitive byte-slice equality (header names are case-insensitive
/// per RFC 9110 §5.1). No allocation, no locale.
pub fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}
