//! ChaCha20 stream cipher (RFC 8439 §2.3-2.4), the keystream half of our
//! from-scratch ChaCha20-Poly1305 AEAD (docs/NETSTACK.md §3).
//!
//! The block function is the same constant-time integer ARX core the kernel
//! (`kernel/src/rng/chacha.rs`) and the per-cell DRBG (`librheo/src/rng.rs`)
//! already carry - no tables, no data-dependent branches, no `.rodata` lookups.
//! It is duplicated here (a third copy, as those two already are) so the `net`
//! crate stays dependency-clean; the DRBG hardcodes nonce/counter 0, whereas the
//! AEAD needs a caller-supplied nonce and an explicit block counter, so this copy
//! exposes both.

/// Produce one 64-byte ChaCha20 keystream block for `(key, counter, nonce)`.
/// RFC 8439 layout: word 12 is the 32-bit block counter, words 13..16 the
/// 96-bit nonce.
pub fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    let c0: u32 = 0x6170_7865;
    let c1: u32 = 0x3320_646e;
    let c2: u32 = 0x7962_2d32;
    let c3: u32 = 0x6b20_6574;
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

/// XOR `buf` in place with the ChaCha20 keystream for `(key, nonce)` starting at
/// block `counter` (RFC 8439 §2.4). Used both to encrypt and to decrypt (the
/// cipher is its own inverse). The AEAD uses `counter = 1` for the payload;
/// block 0 is reserved for the Poly1305 one-time key (`poly1305_key`).
pub fn xor_keystream(key: &[u8; 32], counter: u32, nonce: &[u8; 12], buf: &mut [u8]) {
    let mut ks = [0u8; 64];
    let mut ctr = counter;
    let mut off = 0;
    while off < buf.len() {
        block(key, ctr, nonce, &mut ks);
        let n = core::cmp::min(64, buf.len() - off);
        for i in 0..n {
            buf[off + i] ^= ks[i];
        }
        ctr = ctr.wrapping_add(1);
        off += 64;
    }
}

/// The 32-byte Poly1305 one-time key for an AEAD message: the first 32 bytes of
/// ChaCha20 block 0 under `(key, nonce)` (RFC 8439 §2.6 `poly1305_key_gen`).
pub fn poly1305_key(key: &[u8; 32], nonce: &[u8; 12]) -> [u8; 32] {
    let mut blk = [0u8; 64];
    block(key, 0, nonce, &mut blk);
    let mut k = [0u8; 32];
    k.copy_from_slice(&blk[..32]);
    k
}
