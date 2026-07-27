//! ChaCha20-Poly1305 AEAD (RFC 8439 §2.8), built from scratch on our own
//! ChaCha20 block ([`super::chacha`]) and Poly1305 ([`super::poly1305`]) -
//! docs/NETSTACK.md §3, the one AEAD our hybrid posture owns end to end.
//!
//! Proven against the RFC 8439 §2.8.2 test vector (key/nonce/aad/plaintext ->
//! known ciphertext + tag), a decrypt round trip, and a tampered-tag rejection
//! in the `netcrypto` proof.

use alloc::vec::Vec;

use super::aead::{Aead, Nonce, Tag};
use super::{chacha, poly1305};

/// Append zero bytes to round `len` up to a 16-byte boundary, into `mac`.
fn pad16(mac: &mut Vec<u8>, len: usize) {
    let rem = len % 16;
    if rem != 0 {
        for _ in 0..(16 - rem) {
            mac.push(0);
        }
    }
}

/// Build the Poly1305 MAC input for one AEAD message (RFC 8439 §2.8):
/// `aad || pad16(aad) || ciphertext || pad16(ciphertext) || le64(aad_len) ||
/// le64(ct_len)`.
fn mac_data(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(aad.len() + ciphertext.len() + 32);
    m.extend_from_slice(aad);
    pad16(&mut m, aad.len());
    m.extend_from_slice(ciphertext);
    pad16(&mut m, ciphertext.len());
    m.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    m.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    m
}

/// ChaCha20-Poly1305 with a 256-bit key.
pub struct ChaCha20Poly1305 {
    key: [u8; 32],
}

impl ChaCha20Poly1305 {
    pub fn new(key: [u8; 32]) -> Self {
        ChaCha20Poly1305 { key }
    }
}

impl Aead for ChaCha20Poly1305 {
    fn seal(&self, nonce: &Nonce, aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, Tag) {
        // ChaCha20 encrypts the payload starting at block counter 1; block 0 is
        // the Poly1305 one-time key.
        let otk = chacha::poly1305_key(&self.key, nonce);
        let mut ciphertext = plaintext.to_vec();
        chacha::xor_keystream(&self.key, 1, nonce, &mut ciphertext);
        let tag = poly1305::tag(&otk, &mac_data(aad, &ciphertext));
        (ciphertext, tag)
    }

    fn open(&self, nonce: &Nonce, aad: &[u8], ciphertext: &[u8], tag: &Tag) -> Option<Vec<u8>> {
        // Authenticate BEFORE decrypting (encrypt-then-MAC): recompute the tag
        // over the received ciphertext and compare in constant time.
        let otk = chacha::poly1305_key(&self.key, nonce);
        let expected = poly1305::tag(&otk, &mac_data(aad, ciphertext));
        if !poly1305::verify(&expected, tag) {
            return None;
        }
        let mut plaintext = ciphertext.to_vec();
        chacha::xor_keystream(&self.key, 1, nonce, &mut plaintext);
        Some(plaintext)
    }
}
