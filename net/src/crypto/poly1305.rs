//! Poly1305 one-time authenticator (RFC 8439 §2.5), from scratch
//! (docs/NETSTACK.md §3). This is the MAC half of our ChaCha20-Poly1305 AEAD.
//!
//! It evaluates the 130-bit polynomial `sum(m_i * r^(q-i)) + s  (mod 2^130 - 5)`,
//! then reduces mod `2^128`. The implementation is the well-known public-domain
//! "poly1305-donna" 32-bit reference: the 130-bit values are held as five
//! 26-bit limbs so every partial product fits a `u64`, and the reduction uses
//! `2^130 ≡ 5 (mod 2^130-5)`. No secret-dependent branches or table lookups.
//!
//! Proven against the RFC 8439 §2.5.2 test vector (key/message -> tag) in the
//! `netcrypto` proof. A Poly1305 key is one-time: never authenticate two
//! messages under the same `(r, s)` - the AEAD derives a fresh key per nonce.

const MASK26: u64 = 0x3ff_ffff;

#[inline]
fn le32(b: &[u8]) -> u64 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64
}

/// Compute the 16-byte Poly1305 tag of `msg` under the 32-byte one-time `key`
/// (`r` = key[0..16] clamped, `s` = key[16..32]).
pub fn tag(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    // Clamp r into five 26-bit limbs (RFC 8439 clamp mask 0x0ffffffc0ffffffc...).
    let t0 = le32(&key[0..4]);
    let t1 = le32(&key[4..8]);
    let t2 = le32(&key[8..12]);
    let t3 = le32(&key[12..16]);
    let r0 = t0 & 0x3ff_ffff;
    let r1 = ((t0 >> 26) | (t1 << 6)) & 0x3ff_ff03;
    let r2 = ((t1 >> 20) | (t2 << 12)) & 0x3ff_c0ff;
    let r3 = ((t2 >> 14) | (t3 << 18)) & 0x3f0_3fff;
    let r4 = (t3 >> 8) & 0x00f_ffff;
    // Pre-multiplied limbs for the mod-(2^130-5) fold (each * 5).
    let s1 = r1 * 5;
    let s2 = r2 * 5;
    let s3 = r3 * 5;
    let s4 = r4 * 5;

    // Accumulator h, five 26-bit limbs.
    let (mut h0, mut h1, mut h2, mut h3, mut h4) = (0u64, 0u64, 0u64, 0u64, 0u64);

    let mut off = 0;
    while off < msg.len() {
        let n = core::cmp::min(16, msg.len() - off);
        // A full block carries the implicit high bit at 2^128 (limb 4 bit 24);
        // a final short block instead appends a 0x01 byte inside the buffer.
        let hibit: u64 = if n == 16 { 1 << 24 } else { 0 };
        let mut buf = [0u8; 16];
        buf[..n].copy_from_slice(&msg[off..off + n]);
        if n < 16 {
            buf[n] = 1;
        }
        let m0 = le32(&buf[0..4]);
        let m1 = le32(&buf[4..8]);
        let m2 = le32(&buf[8..12]);
        let m3 = le32(&buf[12..16]);

        // h += m
        h0 += m0 & MASK26;
        h1 += ((m0 >> 26) | (m1 << 6)) & MASK26;
        h2 += ((m1 >> 20) | (m2 << 12)) & MASK26;
        h3 += ((m2 >> 14) | (m3 << 18)) & MASK26;
        h4 += (m3 >> 8) | hibit;

        // h *= r  (mod 2^130 - 5), schoolbook with the *5 folds.
        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;

        // Carry-propagate back into 26-bit limbs.
        let mut c;
        c = d0 >> 26;
        h0 = d0 & MASK26;
        let d1 = d1 + c;
        c = d1 >> 26;
        h1 = d1 & MASK26;
        let d2 = d2 + c;
        c = d2 >> 26;
        h2 = d2 & MASK26;
        let d3 = d3 + c;
        c = d3 >> 26;
        h3 = d3 & MASK26;
        let d4 = d4 + c;
        c = d4 >> 26;
        h4 = d4 & MASK26;
        h0 += c * 5;
        c = h0 >> 26;
        h0 &= MASK26;
        h1 += c;

        off += n;
    }

    // Fully carry h.
    let mut c;
    c = h1 >> 26;
    h1 &= MASK26;
    h2 += c;
    c = h2 >> 26;
    h2 &= MASK26;
    h3 += c;
    c = h3 >> 26;
    h3 &= MASK26;
    h4 += c;
    c = h4 >> 26;
    h4 &= MASK26;
    h0 += c * 5;
    c = h0 >> 26;
    h0 &= MASK26;
    h1 += c;

    // Compute h - (2^130 - 5): g = h + 5, then subtract 2^130.
    let mut g0 = h0 + 5;
    c = g0 >> 26;
    g0 &= MASK26;
    let mut g1 = h1 + c;
    c = g1 >> 26;
    g1 &= MASK26;
    let mut g2 = h2 + c;
    c = g2 >> 26;
    g2 &= MASK26;
    let mut g3 = h3 + c;
    c = g3 >> 26;
    g3 &= MASK26;
    let g4 = h4.wrapping_add(c).wrapping_sub(1 << 26);

    // Constant-time select: if the subtraction borrowed (h < p) keep h, else g.
    let mask = (g4 >> 63).wrapping_sub(1); // borrow -> bit63 set -> mask 0 (keep h)
    let nmask = !mask;
    h0 = (h0 & nmask) | (g0 & mask);
    h1 = (h1 & nmask) | (g1 & mask);
    h2 = (h2 & nmask) | (g2 & mask);
    h3 = (h3 & nmask) | (g3 & mask);
    h4 = (h4 & nmask) | (g4 & mask);

    // Reassemble the four 32-bit words of h (128 bits).
    let mut f0 = (h0 | (h1 << 26)) & 0xffff_ffff;
    let mut f1 = ((h1 >> 6) | (h2 << 20)) & 0xffff_ffff;
    let mut f2 = ((h2 >> 12) | (h3 << 14)) & 0xffff_ffff;
    let mut f3 = ((h3 >> 18) | (h4 << 8)) & 0xffff_ffff;

    // tag = (h + s) mod 2^128.
    let s0 = le32(&key[16..20]);
    let s1w = le32(&key[20..24]);
    let s2w = le32(&key[24..28]);
    let s3w = le32(&key[28..32]);
    let mut f = f0 + s0;
    f0 = f & 0xffff_ffff;
    f = f1 + s1w + (f >> 32);
    f1 = f & 0xffff_ffff;
    f = f2 + s2w + (f >> 32);
    f2 = f & 0xffff_ffff;
    f = f3 + s3w + (f >> 32);
    f3 = f & 0xffff_ffff;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&(f0 as u32).to_le_bytes());
    out[4..8].copy_from_slice(&(f1 as u32).to_le_bytes());
    out[8..12].copy_from_slice(&(f2 as u32).to_le_bytes());
    out[12..16].copy_from_slice(&(f3 as u32).to_le_bytes());
    out
}

/// Constant-time 16-byte tag comparison (no early-out on the first mismatch).
pub fn verify(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}
