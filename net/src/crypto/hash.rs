//! SHA-2 hashing (SHA-256 + SHA-384), wrapping the audited RustCrypto `sha2`
//! crate behind rheo-net's own API (docs/NETSTACK.md §3). Proven against the
//! NIST/RFC 6234 "abc" digests in the `netcrypto` proof.

use sha2::{Digest, Sha256, Sha384};

/// SHA-256 of `data` (32 bytes).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// SHA-384 of `data` (48 bytes).
pub fn sha384(data: &[u8]) -> [u8; 48] {
    let mut h = Sha384::new();
    h.update(data);
    h.finalize().into()
}

/// A streaming SHA-256 (a handshake transcript hash is built incrementally).
pub struct Sha256Hasher(Sha256);

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Hasher {
    pub fn new() -> Self {
        Sha256Hasher(Sha256::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finalize(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}
