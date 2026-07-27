//! Two randomness classes, structurally separated (docs/NETSTACK.md §3).
//!
//! Conflating public randomness with key material is a silent nonce-reuse /
//! key-leak break, so rheo-net keeps them in **distinct types with no bridge**:
//!
//! - **Public randomness** ([`PublicRandom`]) - a fast ChaCha20 side-stream for
//!   **non-secret** values only: DNS transaction ids, cookies, hash seeds,
//!   backoff jitter. Its output is plain integers/bytes. It can NOT produce a
//!   key-schedule input ([`super::kdf::Ikm`]) - there is no `From` and no method
//!   that yields one, so the type system forbids "use a random cookie as a key".
//! - **Key schedules** ([`super::kdf`]) - keys derive via **HKDF over a
//!   transcript**, keyed from the **attested per-cell DRBG** ([`Ikm::from_attested`]
//!   draws from `librheo::rng`, seeded from the kernel DRBG over `SYS_RANDOM`).
//!
//! Both draw ultimately from the attested per-cell root, but through **separate
//! streams and separate types**, so a public value never becomes secret keying
//! material. See [`super::kdf::Ikm`] for the key side.
//!
//! [`Ikm`]: super::kdf::Ikm
//! [`Ikm::from_attested`]: super::kdf::Ikm::from_attested

use core::sync::atomic::{AtomicU64, Ordering};

use super::chacha;

/// The process fork/restore epoch. A fork or checkpoint-restore is a nonce-reuse
/// hazard (the restored image would replay its AEAD counter), so on such an event
/// the cell must call [`bump_fork_epoch`]; any live [`super::aead::SealingKey`]
/// then refuses to seal until reseeded (docs/NETSTACK.md §3, the fork hazard).
static FORK_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The current fork epoch (a `SealingKey` snapshots this at creation).
pub fn fork_epoch() -> u64 {
    FORK_EPOCH.load(Ordering::Relaxed)
}

/// Signal that the cell forked or was restored: bump the epoch so every existing
/// AEAD key must be reseeded before the next seal (prevents a replayed nonce).
pub fn bump_fork_epoch() {
    FORK_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// A fast public-randomness source: a ChaCha20 keystream generator seeded once
/// from the attested per-cell DRBG, then run as a plain side-stream. For
/// **non-secret** values ONLY (nonces/cookies/DNS txids). It intentionally
/// exposes only integers/bytes - never a key type.
pub struct PublicRandom {
    key: [u8; 32],
    counter: u32,
    buf: [u8; 64],
    pos: usize,
}

impl PublicRandom {
    fn refill(&mut self) {
        // Fast key erasure: the first 32 keystream bytes become the next key, so
        // the generator never repeats and past output stays unrecoverable.
        let nonce = [0u8; 12];
        chacha::block(&self.key, self.counter, &nonce, &mut self.buf);
        self.counter = self.counter.wrapping_add(1);
        self.pos = 0;
    }

    /// Fill `dst` with public (non-secret) random bytes.
    pub fn fill(&mut self, dst: &mut [u8]) {
        let mut i = 0;
        while i < dst.len() {
            if self.pos == 64 {
                self.refill();
            }
            let n = core::cmp::min(dst.len() - i, 64 - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            i += n;
        }
    }

    /// A public 16-bit value (e.g. a DNS transaction id).
    pub fn next_u16(&mut self) -> u16 {
        let mut b = [0u8; 2];
        self.fill(&mut b);
        u16::from_le_bytes(b)
    }

    /// A public 32-bit value.
    pub fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill(&mut b);
        u32::from_le_bytes(b)
    }

    /// A public 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}

/// Seed a fresh [`PublicRandom`] from the attested per-cell DRBG (`librheo::rng`).
/// The resulting side-stream is for non-secret values only.
pub fn public_random() -> PublicRandom {
    let mut key = [0u8; 32];
    librheo::rng::fill_bytes(&mut key);
    PublicRandom {
        key,
        counter: 0,
        buf: [0u8; 64],
        pos: 64,
    }
}
