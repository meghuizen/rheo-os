// ChaCha20 block function (RFC 8439 section 2.3). Pure integer ARX - no
// tables, no data-dependent branches, no lookups - so it is constant-time
// and has no cache side channels. This is the same core primitive Linux's
// CRNG uses, which is what makes the getrandom comparison apples to apples.
//
// This file is deliberately dependency-free and no_std-clean so the host
// comparison benchmark (comparison/rng/) can `include!` the exact same code
// it measures against Linux. Regular // comments (not //! module docs) keep
// it includable mid-file.

/// One ChaCha20 quarter-round on the working state.
#[inline(always)]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

#[inline(always)]
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Produce one 64-byte ChaCha20 keystream block for `(key, counter, nonce)`.
/// Layout follows RFC 8439: word 12 is the 32-bit block counter, words
/// 13..16 are the 96-bit nonce.
///
/// On the kernel targets this lives in `.user.text` so a cell can run it in
/// U-mode (the per-cell library-call RNG) without calling into unmapped
/// kernel `.text`; it is panic-free, so there are no hidden bounds-check
/// calls out of that section. The cfg keeps the host comparison build (which
/// `include!`s this file) unaffected.
#[cfg_attr(target_os = "none", unsafe(link_section = ".user.text"))]
#[inline]
pub fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    // "expand 32-byte k"
    const C: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    let mut init = [0u32; 16];
    init[0] = C[0];
    init[1] = C[1];
    init[2] = C[2];
    init[3] = C[3];
    let mut i = 0;
    while i < 8 {
        init[4 + i] = le32(key, i * 4);
        i += 1;
    }
    init[12] = counter;
    init[13] = le32(nonce, 0);
    init[14] = le32(nonce, 4);
    init[15] = le32(nonce, 8);

    let mut w = init;
    // 20 rounds = 10 column/diagonal double-rounds.
    let mut r = 0;
    while r < 10 {
        quarter_round(&mut w, 0, 4, 8, 12);
        quarter_round(&mut w, 1, 5, 9, 13);
        quarter_round(&mut w, 2, 6, 10, 14);
        quarter_round(&mut w, 3, 7, 11, 15);
        quarter_round(&mut w, 0, 5, 10, 15);
        quarter_round(&mut w, 1, 6, 11, 12);
        quarter_round(&mut w, 2, 7, 8, 13);
        quarter_round(&mut w, 3, 4, 9, 14);
        r += 1;
    }

    let mut j = 0;
    while j < 16 {
        let v = w[j].wrapping_add(init[j]);
        let b = v.to_le_bytes();
        out[j * 4] = b[0];
        out[j * 4 + 1] = b[1];
        out[j * 4 + 2] = b[2];
        out[j * 4 + 3] = b[3];
        j += 1;
    }
}
