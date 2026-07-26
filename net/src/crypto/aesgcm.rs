//! AES-GCM AEAD (128- and 256-bit keys), wrapping the audited RustCrypto
//! `aes-gcm` crate behind rheo-net's [`Aead`] seam (docs/NETSTACK.md §3). Built
//! on the **software** AES + GHASH backends (the `aes_force_soft` /
//! `polyval_force_soft` build cfgs, see `net/Cargo.toml`) - the scalar portable
//! path our doctrine wants and the only one that miscompiles-free on all three
//! bare targets. Proven against a NIST/GCM-spec test vector in the `netcrypto`
//! proof.

use alloc::vec::Vec;

use aes_gcm::aead::{Aead as RcAead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm as RcAes128, Aes256Gcm as RcAes256, Nonce as RcNonce};

use super::aead::{Aead, Nonce, Tag};

/// Split the RustCrypto combined `ciphertext || tag(16)` into our detached shape.
fn split_tag(mut combined: Vec<u8>) -> (Vec<u8>, Tag) {
    let tag_start = combined.len() - 16;
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&combined[tag_start..]);
    combined.truncate(tag_start);
    (combined, tag)
}

/// AES-128-GCM.
pub struct Aes128Gcm(RcAes128);

impl Aes128Gcm {
    pub fn new(key: [u8; 16]) -> Self {
        Aes128Gcm(RcAes128::new((&key).into()))
    }
}

impl Aead for Aes128Gcm {
    fn seal(&self, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, Tag) {
        let combined = self
            .0
            .encrypt(
                RcNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("aes-128-gcm seal");
        split_tag(combined)
    }

    fn open(&self, nonce: &Nonce, aad: &[u8], ciphertext: &[u8], tag: &Tag) -> Option<Vec<u8>> {
        let mut combined = Vec::with_capacity(ciphertext.len() + 16);
        combined.extend_from_slice(ciphertext);
        combined.extend_from_slice(tag);
        self.0
            .decrypt(
                RcNonce::from_slice(nonce),
                Payload {
                    msg: &combined,
                    aad,
                },
            )
            .ok()
    }
}

/// AES-256-GCM.
pub struct Aes256Gcm(RcAes256);

impl Aes256Gcm {
    pub fn new(key: [u8; 32]) -> Self {
        Aes256Gcm(RcAes256::new((&key).into()))
    }
}

impl Aead for Aes256Gcm {
    fn seal(&self, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, Tag) {
        let combined = self
            .0
            .encrypt(
                RcNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("aes-256-gcm seal");
        split_tag(combined)
    }

    fn open(&self, nonce: &Nonce, aad: &[u8], ciphertext: &[u8], tag: &Tag) -> Option<Vec<u8>> {
        let mut combined = Vec::with_capacity(ciphertext.len() + 16);
        combined.extend_from_slice(ciphertext);
        combined.extend_from_slice(tag);
        self.0
            .decrypt(
                RcNonce::from_slice(nonce),
                Payload {
                    msg: &combined,
                    aad,
                },
            )
            .ok()
    }
}
