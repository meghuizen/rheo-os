//! The TLS 1.3 key schedule (RFC 8446 §7.1), built on the N3a HKDF primitives
//! (`crypto::kdf`) - docs/NETSTACK.md §15. This is the correctness heart of TLS
//! 1.3: every traffic key, IV, and Finished MAC descends from the Early ->
//! Handshake -> Master secret chain by HKDF-Extract and Derive-Secret
//! (HKDF-Expand-Label over the transcript hash). It is proven **byte-for-byte**
//! against the RFC 8448 §3 known-answer trace in the `nettls` proof.
//!
//! Only the SHA-256 suites (`TLS_AES_128_GCM_SHA256`,
//! `TLS_CHACHA20_POLY1305_SHA256`) are supported, so the hash is SHA-256
//! throughout (`HASH_LEN = 32`). A SHA-384 suite would parameterise this over the
//! hash; deferred (docs/NETSTACK.md §15).

use crate::crypto::hash::sha256;
use crate::crypto::kdf::{self, Ikm};

/// TLS 1.3 with SHA-256: the secret / transcript-hash length.
pub const HASH_LEN: usize = 32;
/// A 32-byte key-schedule secret (SHA-256 output length).
pub type Secret = [u8; 32];

/// The empty-string transcript hash `SHA-256("")` - the context for the two
/// `"derived"` Derive-Secret steps (they have no transcript).
pub fn empty_hash() -> [u8; 32] {
    sha256(&[])
}

/// HKDF-Extract with a raw secret salt (RFC 5869 §2.2). A `0`-length salt and an
/// all-zero salt of the hash length produce the same HMAC (both pad to the block
/// with zeros), so the Early Secret's "salt = 0" is `&[0; 32]` here.
fn extract(salt: &[u8], ikm: &[u8]) -> Secret {
    *kdf::hkdf_extract(salt, &Ikm::import(ikm)).as_bytes()
}

/// The Early Secret: `HKDF-Extract(salt = 0, IKM = PSK)`. With no PSK the IKM is
/// `0^HashLen` (the common 1-RTT case, RFC 8446 §7.1).
pub fn early_secret(psk: Option<&[u8]>) -> Secret {
    let zero = [0u8; HASH_LEN];
    extract(&[0u8; HASH_LEN], psk.unwrap_or(&zero))
}

/// The Handshake Secret: `HKDF-Extract(salt = Derive-Secret(Early, "derived", ""),
/// IKM = (EC)DHE)`. `ecdhe` is the X25519 shared secret (RFC 8446 §7.1).
pub fn handshake_secret(early: &Secret, ecdhe: &[u8]) -> Secret {
    let salt = derive_secret(early, b"derived", &empty_hash());
    extract(&salt, ecdhe)
}

/// The Master Secret: `HKDF-Extract(salt = Derive-Secret(Handshake, "derived",
/// ""), IKM = 0)` (RFC 8446 §7.1).
pub fn master_secret(handshake: &Secret) -> Secret {
    let salt = derive_secret(handshake, b"derived", &empty_hash());
    extract(&salt, &[0u8; HASH_LEN])
}

/// Derive-Secret (RFC 8446 §7.1): `HKDF-Expand-Label(secret, label,
/// Transcript-Hash(messages), HashLen)`. `transcript_hash` is the caller's
/// running SHA-256 over the handshake messages (or [`empty_hash`] for "derived").
pub fn derive_secret(secret: &Secret, label: &[u8], transcript_hash: &[u8]) -> Secret {
    let mut out = [0u8; HASH_LEN];
    kdf::hkdf_expand_label_secret(secret, label, transcript_hash, &mut out)
        .expect("derive_secret: HashLen output cannot be too long");
    out
}

/// A record-protection write key: `HKDF-Expand-Label(secret, "key", "", key_len)`
/// (RFC 8446 §7.3). `key_len` is 16 for AES-128-GCM, 32 for ChaCha20-Poly1305.
pub fn traffic_key(secret: &Secret, key_len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; key_len];
    kdf::hkdf_expand_label_secret(secret, b"key", &[], &mut out).expect("traffic_key");
    out
}

/// A record-protection write IV: `HKDF-Expand-Label(secret, "iv", "", 12)` (the
/// 96-bit AEAD nonce base, RFC 8446 §7.3).
pub fn traffic_iv(secret: &Secret) -> [u8; 12] {
    let mut out = [0u8; 12];
    kdf::hkdf_expand_label_secret(secret, b"iv", &[], &mut out).expect("traffic_iv");
    out
}

/// The Finished MAC key: `HKDF-Expand-Label(base_key, "finished", "", HashLen)`
/// (RFC 8446 §4.4.4). `base_key` is the client/server handshake traffic secret.
pub fn finished_key(base_key: &Secret) -> Secret {
    let mut out = [0u8; HASH_LEN];
    kdf::hkdf_expand_label_secret(base_key, b"finished", &[], &mut out).expect("finished_key");
    out
}

/// The Finished `verify_data`: `HMAC-SHA256(finished_key,
/// Transcript-Hash(messages))` (RFC 8446 §4.4.4). `transcript_hash` runs through
/// the message *before* this Finished (CertificateVerify for the server's,
/// server Finished for the client's).
pub fn verify_data(finished_key: &Secret, transcript_hash: &[u8]) -> [u8; 32] {
    hmac_sha256(finished_key, transcript_hash)
}

/// HMAC-SHA256 (RFC 2104), from scratch over the audited `sha2` SHA-256 - the
/// only MAC TLS 1.3's key schedule needs beyond HKDF (which is itself HMAC-based,
/// but the `hkdf` crate does not expose a bare HMAC). Small and side-effect-free;
/// the block size is 64 bytes. Poly1305 set the from-scratch-MAC precedent (N3a).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    // Keys longer than the block are hashed first; ours are always <= 32.
    let mut k0 = [0u8; BLOCK];
    if key.len() > BLOCK {
        let kh = sha256(key);
        k0[..32].copy_from_slice(&kh);
    } else {
        k0[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k0[i];
        opad[i] ^= k0[i];
    }
    // inner = SHA256(ipad || msg)
    let mut inner = alloc::vec::Vec::with_capacity(BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256(&inner);
    // outer = SHA256(opad || inner_hash)
    let mut outer = [0u8; BLOCK + 32];
    outer[..BLOCK].copy_from_slice(&opad);
    outer[BLOCK..].copy_from_slice(&inner_hash);
    sha256(&outer)
}
