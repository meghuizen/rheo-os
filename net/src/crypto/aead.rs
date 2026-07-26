//! The AEAD seam + the **nonce-reuse hazard guard** (docs/NETSTACK.md §3).
//!
//! A single (key, nonce) pair must NEVER encrypt two different messages under
//! ChaCha20-Poly1305 or AES-GCM - it is a catastrophic, silent break (the
//! keystreams XOR out, and the Poly1305/GHASH one-time key leaks). The API here
//! makes that structurally hard:
//!
//! - The low-level [`Aead::seal`]/[`Aead::open`] take an explicit nonce - used
//!   only to replay published RFC/NIST **test vectors**.
//! - Real code uses [`SealingKey`], which **owns a monotonic counter** and never
//!   lets the caller choose a nonce: each `seal` consumes the next counter value,
//!   so the same nonce cannot be produced twice, and the sequence refuses to wrap.
//! - Fork / checkpoint-restore is the other nonce-reuse hazard (a restored image
//!   replays its counter). A `SealingKey` captures the process **fork epoch** at
//!   creation (`crypto::rand::fork_epoch`); after a fork or restore the cell bumps
//!   the epoch, and any surviving `SealingKey` then **refuses to seal**
//!   ([`NonceError::ReseedRequired`]) until a fresh key is installed. Full
//!   checkpoint integration is later; the API already forbids a replayed nonce.

use alloc::vec::Vec;

use super::rand;

/// A 96-bit AEAD nonce (RFC 8439 / NIST GCM both use 12 bytes).
pub type Nonce = [u8; 12];
/// A 128-bit authentication tag.
pub type Tag = [u8; 16];

/// An authenticated-encryption primitive over a 12-byte nonce and a 16-byte tag.
/// Implemented by [`super::chachapoly::ChaCha20Poly1305`] and
/// [`super::aesgcm`] `Aes128Gcm` / `Aes256Gcm`.
pub trait Aead {
    /// Encrypt `plaintext` with `aad` authenticated, returning `(ciphertext, tag)`.
    fn seal(&self, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, Tag);
    /// Verify `tag` and decrypt, returning the plaintext, or `None` if the tag
    /// (or aad/ciphertext) is wrong - a constant-time authentication check.
    fn open(&self, nonce: &Nonce, aad: &[u8], ciphertext: &[u8], tag: &Tag) -> Option<Vec<u8>>;
}

/// Why a nonce-safe seal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceError {
    /// The 64-bit message counter is exhausted - rekey (never reuse a nonce).
    Exhausted,
    /// The fork epoch advanced under this key (a fork/restore happened); the key
    /// must be reseeded before any further sealing, or a (key, nonce) could replay.
    ReseedRequired,
}

/// A message sealed by a [`SealingKey`]: the nonce it chose plus the outputs.
/// The receiver needs the nonce to open, so it travels with the ciphertext.
pub struct Sealed {
    pub nonce: Nonce,
    pub ciphertext: Vec<u8>,
    pub tag: Tag,
}

/// An AEAD key that owns its nonce sequence, so a nonce can never be reused.
///
/// The 96-bit nonce is `iv_prefix(4) || counter(8, big-endian)`; the counter
/// strictly increments per `seal`, so no two messages share a nonce, and the
/// key refuses to seal once the counter would wrap or after a fork/restore.
pub struct SealingKey<A: Aead> {
    aead: A,
    iv_prefix: [u8; 4],
    counter: u64,
    epoch: u64,
}

impl<A: Aead> SealingKey<A> {
    /// Bind `aead` into a nonce-owning key. `iv_prefix` is a fixed 32-bit field
    /// (0 is fine for a single stream; distinct values separate parallel streams
    /// under the same key). Captures the current fork epoch.
    pub fn new(aead: A, iv_prefix: [u8; 4]) -> Self {
        SealingKey {
            aead,
            iv_prefix,
            counter: 0,
            epoch: rand::fork_epoch(),
        }
    }

    fn next_nonce(&mut self) -> Result<Nonce, NonceError> {
        if rand::fork_epoch() != self.epoch {
            return Err(NonceError::ReseedRequired);
        }
        let n = self.counter;
        self.counter = self.counter.checked_add(1).ok_or(NonceError::Exhausted)?;
        let mut nonce = [0u8; 12];
        nonce[0..4].copy_from_slice(&self.iv_prefix);
        nonce[4..12].copy_from_slice(&n.to_be_bytes());
        Ok(nonce)
    }

    /// Seal `plaintext` under the next nonce in the sequence. The nonce is chosen
    /// by the key (never the caller) so it cannot be replayed.
    pub fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed, NonceError> {
        let nonce = self.next_nonce()?;
        let (ciphertext, tag) = self.aead.seal(&nonce, aad, plaintext);
        Ok(Sealed {
            nonce,
            ciphertext,
            tag,
        })
    }

    /// How many messages this key has sealed (the current counter).
    pub fn messages_sealed(&self) -> u64 {
        self.counter
    }
}

/// The opening counterpart: verify + decrypt a [`Sealed`] message. Opening takes
/// the nonce from the wire (there is no reuse hazard on the receive side).
pub struct OpeningKey<A: Aead> {
    aead: A,
}

impl<A: Aead> OpeningKey<A> {
    pub fn new(aead: A) -> Self {
        OpeningKey { aead }
    }

    /// Verify and decrypt, or `None` on any authentication failure.
    pub fn open(&self, nonce: &Nonce, aad: &[u8], ciphertext: &[u8], tag: &Tag) -> Option<Vec<u8>> {
        self.aead.open(nonce, aad, ciphertext, tag)
    }
}
