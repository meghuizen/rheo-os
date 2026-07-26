//! Ed25519 signatures (RFC 8032), wrapping the audited `ed25519-dalek` crate
//! (docs/NETSTACK.md §3). Deterministic (RFC 8032) signing, so a known seed +
//! message yields the RFC signature. Proven against RFC 8032 §7.1 test vectors
//! (sign + verify, and a tampered-signature / tampered-message rejection) in the
//! `netcrypto` proof.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Derive the 32-byte Ed25519 public key from a 32-byte secret seed.
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Sign `message` with the secret seed, returning the 64-byte signature
/// (deterministic per RFC 8032).
pub fn sign(seed: &[u8; 32], message: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(seed).sign(message).to_bytes()
}

/// Verify a 64-byte signature over `message` under a 32-byte public key. Returns
/// `false` on a bad public key encoding or a failed check (strict verification,
/// rejecting non-canonical / small-order points).
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let sig = Signature::from_bytes(signature);
    vk.verify_strict(message, &sig).is_ok()
}
