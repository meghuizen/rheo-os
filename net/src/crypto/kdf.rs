//! HKDF key schedules (RFC 5869, HMAC-SHA256), wrapping the audited RustCrypto
//! `hkdf` crate (docs/NETSTACK.md §3). This is the **key side** of the two
//! randomness classes: keys derive here, from the **attested per-cell DRBG** or
//! an imported pre-shared / DH secret, via HKDF over a transcript - never from
//! the fast public RNG (see [`super::rand`]).
//!
//! The type barrier: [`Ikm`] (secret input keying material) is constructible
//! **only** from the attested DRBG ([`Ikm::from_attested`]) or an explicit
//! external secret ([`Ikm::import`]) - there is no path from [`super::rand::PublicRandom`],
//! so public randomness cannot be smuggled in as a key. Proven against RFC 5869
//! Test Case 1 in the `netcrypto` proof.

use alloc::vec::Vec;

use hkdf::Hkdf;
use sha2::Sha256;

/// HKDF-Expand failed: the requested output length exceeds `255 * HashLen`
/// (RFC 5869 §2.3). The only failure mode HKDF has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTooLong;

/// Secret input keying material for a key schedule. Its only constructors are
/// the attested DRBG or an explicit imported secret - **not** public randomness.
pub struct Ikm(Vec<u8>);

impl Ikm {
    /// Draw 32 bytes from the attested per-cell DRBG (`librheo::rng`, seeded from
    /// the kernel DRBG). This is the randomness class keys are allowed to use.
    pub fn from_attested() -> Ikm {
        let mut b = [0u8; 32];
        librheo::rng::fill_bytes(&mut b);
        Ikm(b.to_vec())
    }

    /// Import an explicit external secret: a pre-shared key, a Diffie-Hellman
    /// shared secret ([`super::kx`]), or a published test vector. Deliberately
    /// takes raw bytes the caller already holds as secret - it is never fed the
    /// output of [`super::rand::PublicRandom`].
    pub fn import(bytes: &[u8]) -> Ikm {
        Ikm(bytes.to_vec())
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }
}

/// A pseudorandom key: the 32-byte HKDF-Extract output. Secret.
pub struct Prk(Hkdf<Sha256>, [u8; 32]);

impl Prk {
    /// The raw 32-byte PRK (for the RFC 5869 vector check; normally kept secret).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.1
    }
}

/// HKDF-Extract (RFC 5869 §2.2): `PRK = HMAC-SHA256(salt, IKM)`.
pub fn hkdf_extract(salt: &[u8], ikm: &Ikm) -> Prk {
    let (prk_bytes, hk) = Hkdf::<Sha256>::extract(Some(salt), ikm.expose());
    let mut prk = [0u8; 32];
    prk.copy_from_slice(&prk_bytes);
    Prk(hk, prk)
}

/// HKDF-Expand (RFC 5869 §2.3): write `out.len()` bytes of output keying
/// material for `info`. Fails only if `out` is longer than `255 * HashLen`.
pub fn hkdf_expand(prk: &Prk, info: &[u8], out: &mut [u8]) -> Result<(), OutputTooLong> {
    prk.0.expand(info, out).map_err(|_| OutputTooLong)
}

/// HKDF-Expand-Label (TLS 1.3, RFC 8446 §7.1): expand with a structured
/// `HkdfLabel { length, "tls13 " || label, context }`. The transcript hash is
/// the `context` - this is how a TLS 1.3 key schedule binds keys to the
/// handshake transcript (the design's "HKDF over the handshake transcript").
pub fn hkdf_expand_label(
    prk: &Prk,
    label: &[u8],
    context: &[u8],
    out: &mut [u8],
) -> Result<(), OutputTooLong> {
    let mut full = Vec::with_capacity(6 + label.len());
    full.extend_from_slice(b"tls13 ");
    full.extend_from_slice(label);
    let mut hkdf_label = Vec::with_capacity(2 + 1 + full.len() + 1 + context.len());
    hkdf_label.extend_from_slice(&(out.len() as u16).to_be_bytes());
    hkdf_label.push(full.len() as u8);
    hkdf_label.extend_from_slice(&full);
    hkdf_label.push(context.len() as u8);
    hkdf_label.extend_from_slice(context);
    hkdf_expand(prk, &hkdf_label, out)
}
