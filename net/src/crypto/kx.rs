//! X25519 Diffie-Hellman key exchange (RFC 7748), wrapping the audited
//! `x25519-dalek` crate (docs/NETSTACK.md §3). The DH shared secret feeds a key
//! schedule as an imported [`super::kdf::Ikm`] - never the fast public RNG.
//! Proven against the RFC 7748 §5.2 scalar-mult and §6.1 DH test vectors in the
//! `netcrypto` proof.

use x25519_dalek::{PublicKey, StaticSecret};

/// The X25519 scalar multiplication `scalar * u` (RFC 7748 §5). Used both for the
/// raw §5.2 vector and, with `u` = the base point, to derive a public key.
pub fn x25519(scalar: [u8; 32], point: [u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(scalar, point)
}

/// The Curve25519 base point `u = 9`.
pub const BASE: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

/// Derive the X25519 public key for a secret scalar (`scalar * base`).
pub fn public_key(secret: [u8; 32]) -> [u8; 32] {
    let sk = StaticSecret::from(secret);
    PublicKey::from(&sk).to_bytes()
}

/// Compute the DH shared secret from our secret and the peer's public key. The
/// result is secret keying material - wrap it as [`super::kdf::Ikm::import`],
/// never as public randomness.
pub fn diffie_hellman(secret: [u8; 32], peer_public: [u8; 32]) -> [u8; 32] {
    let sk = StaticSecret::from(secret);
    let pk = PublicKey::from(peer_public);
    sk.diffie_hellman(&pk).to_bytes()
}
