// ChaCha20 block function (RFC 8439 section 2.3). Pure integer ARX - no
// tables, no data-dependent branches, no lookups - so it is constant-time
// and has no cache side channels. This is the same core primitive Linux's
// CRNG uses, which is what makes the getrandom comparison apples to apples.
//
// Written with 16 scalar locals rather than arrays (registers + immediates,
// no `.rodata` pool, no memcpy/memset) - fast and self-contained.
//
// Dependency-free and includable mid-file by the host comparison benchmark
// (comparison/rng/), so `//` comments, not `//!`.

/// Produce one 64-byte ChaCha20 keystream block for `(key, counter, nonce)`.
/// RFC 8439 layout: word 12 is the 32-bit block counter, words 13..16 the
/// 96-bit nonce.
#[inline]
pub fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    // Constants "expand 32-byte k" and the key/nonce words, all as scalars.
    let c0: u32 = 0x6170_7865;
    let c1: u32 = 0x3320_646e;
    let c2: u32 = 0x7962_2d32;
    let c3: u32 = 0x6b20_6574;
    // Read little-endian words with *constant* indices into the fixed-size
    // arrays, so there is no slice bounds check (and thus no panic branch
    // calling into kernel `.text`, which a U-mode cell cannot reach).
    macro_rules! le {
        ($a:expr, $i:literal) => {
            u32::from_le_bytes([$a[$i], $a[$i + 1], $a[$i + 2], $a[$i + 3]])
        };
    }
    let k0 = le!(key, 0);
    let k1 = le!(key, 4);
    let k2 = le!(key, 8);
    let k3 = le!(key, 12);
    let k4 = le!(key, 16);
    let k5 = le!(key, 20);
    let k6 = le!(key, 24);
    let k7 = le!(key, 28);
    let n0 = le!(nonce, 0);
    let n1 = le!(nonce, 4);
    let n2 = le!(nonce, 8);

    let (mut x0, mut x1, mut x2, mut x3) = (c0, c1, c2, c3);
    let (mut x4, mut x5, mut x6, mut x7) = (k0, k1, k2, k3);
    let (mut x8, mut x9, mut x10, mut x11) = (k4, k5, k6, k7);
    let (mut x12, mut x13, mut x14, mut x15) = (counter, n0, n1, n2);

    macro_rules! qr {
        ($a:ident, $b:ident, $c:ident, $d:ident) => {{
            $a = $a.wrapping_add($b);
            $d = ($d ^ $a).rotate_left(16);
            $c = $c.wrapping_add($d);
            $b = ($b ^ $c).rotate_left(12);
            $a = $a.wrapping_add($b);
            $d = ($d ^ $a).rotate_left(8);
            $c = $c.wrapping_add($d);
            $b = ($b ^ $c).rotate_left(7);
        }};
    }

    // 20 rounds = 10 column/diagonal double-rounds.
    let mut r = 0;
    while r < 10 {
        qr!(x0, x4, x8, x12);
        qr!(x1, x5, x9, x13);
        qr!(x2, x6, x10, x14);
        qr!(x3, x7, x11, x15);
        qr!(x0, x5, x10, x15);
        qr!(x1, x6, x11, x12);
        qr!(x2, x7, x8, x13);
        qr!(x3, x4, x9, x14);
        r += 1;
    }

    // Add the original input words and serialise little-endian. Constant
    // indices into `out`, so no bounds-check panic branch and no memcpy.
    macro_rules! st {
        ($i:expr, $v:expr) => {{
            let b = ($v).to_le_bytes();
            out[$i] = b[0];
            out[$i + 1] = b[1];
            out[$i + 2] = b[2];
            out[$i + 3] = b[3];
        }};
    }
    st!(0, x0.wrapping_add(c0));
    st!(4, x1.wrapping_add(c1));
    st!(8, x2.wrapping_add(c2));
    st!(12, x3.wrapping_add(c3));
    st!(16, x4.wrapping_add(k0));
    st!(20, x5.wrapping_add(k1));
    st!(24, x6.wrapping_add(k2));
    st!(28, x7.wrapping_add(k3));
    st!(32, x8.wrapping_add(k4));
    st!(36, x9.wrapping_add(k5));
    st!(40, x10.wrapping_add(k6));
    st!(44, x11.wrapping_add(k7));
    st!(48, x12.wrapping_add(counter));
    st!(52, x13.wrapping_add(n0));
    st!(56, x14.wrapping_add(n1));
    st!(60, x15.wrapping_add(n2));
}
