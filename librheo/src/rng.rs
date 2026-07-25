//! Per-cell cryptographic randomness as a **library call** (docs/
//! TIME-IDENTITY.md 4, docs/LIBRHEO.md): a ChaCha20 DRBG with fast key erasure
//! that lives entirely in the cell. It is seeded **once** at startup by
//! drawing 32 bytes over the kernel DRBG (`SYS_RANDOM`); every draw after that
//! is a plain function call over the cell's own state - no syscall on the fast
//! path. This finally realizes in a loaded cell the model the kernel `rng`
//! test proves in kernel context.
//!
//! The ChaCha20 block function is ported from `kernel/src/rng/chacha.rs`
//! (RFC 8439 section 2.3): pure integer ARX, no tables, no data-dependent
//! branches, so it is constant-time with no cache side channels.

use core::sync::atomic::{AtomicBool, Ordering};

/// Produce one 64-byte ChaCha20 keystream block for `(key, counter, nonce)`.
fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
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

/// Output bytes handed out between refills.
const OUT: usize = 256;
/// Keystream bytes per refill: 32 (rekey) + OUT, rounded up to whole blocks.
const KS_BYTES: usize = ((32 + OUT).div_ceil(64)) * 64;

/// A ChaCha20 DRBG with fast key erasure.
struct Drbg {
    key: [u8; 32],
    buf: [u8; OUT],
    pos: usize,
}

impl Drbg {
    const fn zero() -> Drbg {
        Drbg {
            key: [0; 32],
            buf: [0; OUT],
            pos: OUT,
        }
    }

    /// Refill `buf`, re-keying from the first 32 keystream bytes (forward
    /// secrecy: the old key and all past output become unrecoverable).
    fn refill(&mut self) {
        let mut ks = [0u8; KS_BYTES];
        let nonce = [0u8; 12];
        let mut ctr = 0u32;
        let mut off = 0;
        while off < KS_BYTES {
            let mut blk = [0u8; 64];
            block(&self.key, ctr, &nonce, &mut blk);
            ks[off..off + 64].copy_from_slice(&blk);
            ctr += 1;
            off += 64;
        }
        self.key.copy_from_slice(&ks[..32]);
        self.buf.copy_from_slice(&ks[32..32 + OUT]);
        self.pos = 0;
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut i = 0;
        while i < dst.len() {
            if self.pos == OUT {
                self.refill();
            }
            let n = core::cmp::min(dst.len() - i, OUT - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }
}

static mut DRBG: Drbg = Drbg::zero();
static SEEDED: AtomicBool = AtomicBool::new(false);

/// Seed the cell's DRBG once, drawing a 256-bit key over `SYS_RANDOM` (the
/// kernel DRBG). Called from `_start`.
pub fn init() {
    let mut key = [0u8; 32];
    for chunk in key.chunks_mut(8) {
        chunk.copy_from_slice(&crate::sys::random_u64().to_le_bytes());
    }
    // SAFETY: single-CPU cooperative cell; init runs once before any draw.
    unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DRBG);
        d.key = key;
        d.pos = OUT;
    }
    SEEDED.store(true, Ordering::Relaxed);
}

/// Fill `dst` with random bytes from the cell's DRBG (no syscall).
pub fn fill_bytes(dst: &mut [u8]) {
    debug_assert!(SEEDED.load(Ordering::Relaxed), "rng used before init");
    // SAFETY: single-CPU cooperative; no concurrent access to the DRBG.
    unsafe { (*core::ptr::addr_of_mut!(DRBG)).fill_bytes(dst) };
}

/// Next 64 random bits from the cell's DRBG (no syscall).
pub fn next_u64() -> u64 {
    let mut b = [0u8; 8];
    fill_bytes(&mut b);
    u64::from_le_bytes(b)
}
