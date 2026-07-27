//! The TLS 1.3 record layer (RFC 8446 §5) over the N3a AEADs - docs/NETSTACK.md
//! §15. A protected record is `TLSCiphertext { opaque_type=23,
//! legacy_version=0x0303, length, AEAD-Encrypt(nonce, aad, inner_plaintext) }`,
//! where the `inner_plaintext` is `content || real_content_type(1) || padding`
//! and the `aad` is the 5-byte record header (RFC 8446 §5.2).
//!
//! The **per-record nonce** (RFC 8446 §5.3) is the classic footgun: the 64-bit
//! record sequence number, left-padded with zeros to the IV length (12), is
//! XORed into the static write IV. Sequence numbers are per-key and reset to 0
//! when the traffic keys change (handshake -> application). [`RecordKeys`] owns
//! the sequence counter so a caller never has to construct the nonce by hand.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::{CipherSuite, TlsError};
use crate::crypto::aead::Aead;
use crate::crypto::{aesgcm, chachapoly};

/// TLS content types (RFC 8446 §5). Handshake and application_data are the ones
/// this slice carries; alert is recognised so a peer close is not misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Handshake,
    ApplicationData,
    Alert,
    ChangeCipherSpec,
}

impl ContentType {
    pub fn to_u8(self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }
    pub fn from_u8(v: u8) -> Option<ContentType> {
        match v {
            20 => Some(ContentType::ChangeCipherSpec),
            21 => Some(ContentType::Alert),
            22 => Some(ContentType::Handshake),
            23 => Some(ContentType::ApplicationData),
            _ => None,
        }
    }
}

/// The AEAD + write IV + sequence counter protecting one direction of one epoch
/// (handshake or application). A fresh `RecordKeys` starts at sequence 0.
pub struct RecordKeys {
    aead: Box<dyn Aead>,
    iv: [u8; 12],
    seq: u64,
}

impl RecordKeys {
    /// Build the keys for `suite` from a traffic secret's write key + IV
    /// (`super::keyschedule::traffic_key`/`traffic_iv`). The AEAD is chosen by the
    /// negotiated cipher suite.
    pub fn new(suite: CipherSuite, key: &[u8], iv: [u8; 12]) -> RecordKeys {
        let aead: Box<dyn Aead> = match suite {
            CipherSuite::Aes128GcmSha256 => {
                let mut k = [0u8; 16];
                k.copy_from_slice(key);
                Box::new(aesgcm::Aes128Gcm::new(k))
            }
            CipherSuite::ChaCha20Poly1305Sha256 => {
                let mut k = [0u8; 32];
                k.copy_from_slice(key);
                Box::new(chachapoly::ChaCha20Poly1305::new(k))
            }
        };
        RecordKeys { aead, iv, seq: 0 }
    }

    /// The per-record nonce (RFC 8446 §5.3): the sequence number, left-padded to
    /// 12 bytes, XORed into the static write IV. The IV's high 4 bytes are never
    /// touched (the seq is only 8 bytes, right-aligned).
    fn nonce(&self) -> [u8; 12] {
        let mut n = self.iv;
        let s = self.seq.to_be_bytes();
        for i in 0..8 {
            n[4 + i] ^= s[i];
        }
        n
    }

    /// The 5-byte record header used as AEAD additional data:
    /// `opaque_type(23) || legacy_record_version(0x0303) || length`, where
    /// `length` is the ciphertext length (`inner_plaintext + 16-byte tag`).
    fn aad(ct_len: usize) -> [u8; 5] {
        let l = ct_len as u16;
        [23, 0x03, 0x03, (l >> 8) as u8, l as u8]
    }

    /// Encrypt one record: append the true content type to the plaintext, seal it
    /// under the next nonce, and frame it as a full `TLSCiphertext`. Advances the
    /// sequence number.
    pub fn encrypt(&mut self, content_type: ContentType, plaintext: &[u8]) -> Vec<u8> {
        // TLSInnerPlaintext = content || content_type(1) || zero padding (none).
        let mut inner = Vec::with_capacity(plaintext.len() + 1);
        inner.extend_from_slice(plaintext);
        inner.push(content_type.to_u8());

        let ct_len = inner.len() + 16; // + AEAD tag
        let aad = Self::aad(ct_len);
        let nonce = self.nonce();
        let (ciphertext, tag) = self.aead.seal(&nonce, &aad, &inner);
        self.seq += 1;

        let mut record = Vec::with_capacity(5 + ct_len);
        record.extend_from_slice(&aad);
        record.extend_from_slice(&ciphertext);
        record.extend_from_slice(&tag);
        record
    }

    /// Decrypt one full `TLSCiphertext` record: verify + open under the next
    /// nonce, strip the trailing content-type byte (and any zero padding), and
    /// return `(real_content_type, content)`. Advances the sequence number only on
    /// a successful open (a failed AEAD is a fatal error - the caller aborts).
    pub fn decrypt(&mut self, record: &[u8]) -> Result<(ContentType, Vec<u8>), TlsError> {
        if record.len() < 5 + 16 {
            return Err(TlsError::Decode);
        }
        let header = &record[..5];
        let body = &record[5..];
        if body.len() < 16 {
            return Err(TlsError::Decode);
        }
        let (ciphertext, tag_bytes) = body.split_at(body.len() - 16);
        let mut tag = [0u8; 16];
        tag.copy_from_slice(tag_bytes);
        let nonce = self.nonce();
        let mut inner = self
            .aead
            .open(&nonce, header, ciphertext, &tag)
            .ok_or(TlsError::BadRecordMac)?;
        self.seq += 1;

        // Strip trailing zero padding, then the one content-type byte.
        while matches!(inner.last(), Some(0)) {
            inner.pop();
        }
        let ct = inner.pop().ok_or(TlsError::Decode)?;
        let content_type = ContentType::from_u8(ct).ok_or(TlsError::Decode)?;
        Ok((content_type, inner))
    }
}
