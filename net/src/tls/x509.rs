//! Minimal X.509 (RFC 5280) - a from-scratch DER walk that extracts a
//! certificate's **subject public key** and **signature** so the signature can be
//! verified (docs/NETSTACK.md §15). This is deliberately *minimal*: it parses the
//! `tbsCertificate` (the signed bytes), the `SubjectPublicKeyInfo` (an Ed25519
//! key), and the outer `signatureValue`, and verifies the certificate's own
//! Ed25519 signature (or a CertificateVerify signature, via the extracted key).
//!
//! **Full chain / path / name validation is DEFERRED** (docs/NETSTACK.md §15):
//! no issuer-chain walk, no validity-date check, no name/SAN matching, no basic-
//! constraints/EKU enforcement. That is sufficient for the downstream Tor/onion
//! consumer, which validates peer identity out of band (the descriptor's own
//! signature over the identity key), so a full PKI path is not the trust anchor
//! there - only that the CertificateVerify was signed by the presented key.
//!
//! Only **Ed25519** SPKIs are extracted (OID 1.3.101.112). ECDSA/RSA SPKI parsing
//! and their signature verification are deferred with the wider X.509 work.

use super::TlsError;
use crate::crypto::sign;

/// The Ed25519 algorithm identifier `SEQUENCE { OID 1.3.101.112 }` in DER:
/// `30 05 06 03 2b 65 70`. Its presence marks an Ed25519 SubjectPublicKeyInfo.
const ED25519_ALG_ID: [u8; 7] = [0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

/// A parsed certificate: the signed `tbsCertificate` bytes, the Ed25519 subject
/// public key, and the outer signature value. Borrows the input DER.
pub struct Certificate<'a> {
    /// The exact `tbsCertificate` TLV bytes - what `signatureValue` signs.
    pub tbs: &'a [u8],
    /// The 32-byte Ed25519 subject public key from the SubjectPublicKeyInfo.
    pub subject_public_key: [u8; 32],
    /// The 64-byte Ed25519 signature from the outer `signatureValue` BIT STRING.
    pub signature: [u8; 64],
}

impl Certificate<'_> {
    /// Verify the certificate's own signature under its subject public key. For a
    /// self-signed cert this proves issuer == subject held the key; used as the
    /// minimal signature check (chain validation is deferred). Constant-time via
    /// `ed25519-dalek`'s strict verify.
    pub fn verify_self_signed(&self) -> bool {
        sign::verify(&self.subject_public_key, self.tbs, &self.signature)
    }
}

/// A DER cursor over a byte slice: reads tag-length-value triples.
struct Der<'a> {
    buf: &'a [u8],
    pos: usize,
}

/// One parsed TLV: its tag, the byte range of its content, and the end offset of
/// the whole TLV (so the caller can capture the full TLV bytes when needed).
struct Tlv {
    tag: u8,
    content_start: usize,
    content_end: usize,
    tlv_end: usize,
}

impl<'a> Der<'a> {
    fn new(buf: &'a [u8]) -> Der<'a> {
        Der { buf, pos: 0 }
    }

    /// Read the next TLV at the cursor, advancing past it. Handles DER short-form
    /// and long-form (1-4 length octets) lengths; rejects anything malformed.
    fn read(&mut self) -> Result<Tlv, TlsError> {
        let start = self.pos;
        if start + 2 > self.buf.len() {
            return Err(TlsError::Decode);
        }
        let tag = self.buf[start];
        let len_byte = self.buf[start + 1];
        let (content_start, len) = if len_byte < 0x80 {
            (start + 2, len_byte as usize)
        } else {
            let n = (len_byte & 0x7f) as usize;
            if n == 0 || n > 4 || start + 2 + n > self.buf.len() {
                return Err(TlsError::Decode);
            }
            let mut l = 0usize;
            for i in 0..n {
                l = (l << 8) | self.buf[start + 2 + i] as usize;
            }
            (start + 2 + n, l)
        };
        let content_end = content_start.checked_add(len).ok_or(TlsError::Decode)?;
        if content_end > self.buf.len() {
            return Err(TlsError::Decode);
        }
        self.pos = content_end;
        Ok(Tlv {
            tag,
            content_start,
            content_end,
            tlv_end: content_end,
        })
    }
}

/// Parse a DER-encoded X.509 certificate far enough to extract the signed bytes,
/// the Ed25519 subject public key, and the signature. Errors on any structural
/// surprise or a non-Ed25519 SPKI.
pub fn parse(der: &[u8]) -> Result<Certificate<'_>, TlsError> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let mut top = Der::new(der);
    let cert = top.read()?;
    if cert.tag != 0x30 {
        return Err(TlsError::Decode);
    }
    let mut inner = Der {
        buf: der,
        pos: cert.content_start,
    };

    // tbsCertificate: its full TLV bytes (tag..end) are what `signatureValue`
    // signs. It is the first element of the outer SEQUENCE, so it starts at the
    // outer content start.
    let tbs = inner.read()?;
    if tbs.tag != 0x30 {
        return Err(TlsError::Decode);
    }
    let tbs_full = &der[cert.content_start..tbs.tlv_end];

    // signatureAlgorithm (skip) then signatureValue BIT STRING.
    let _sig_alg = inner.read()?;
    let sig_val = inner.read()?;
    if sig_val.tag != 0x03 {
        return Err(TlsError::Decode);
    }
    // BIT STRING content = unused-bits(1, = 0) || 64-byte signature.
    let sig_content = &der[sig_val.content_start..sig_val.content_end];
    if sig_content.len() != 65 || sig_content[0] != 0x00 {
        return Err(TlsError::Decode);
    }
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&sig_content[1..]);

    // Walk tbsCertificate to the SubjectPublicKeyInfo. RFC 5280 order:
    // [0]version?, serial, sigalg, issuer, validity, subject, spki, [3]extensions?.
    let mut tbs_cur = Der {
        buf: der,
        pos: tbs.content_start,
    };
    let mut spki: Option<Tlv> = None;
    let mut prev: Option<Tlv> = None;
    while tbs_cur.pos < tbs.content_end {
        let el = tbs_cur.read()?;
        if el.tag == 0xA3 {
            // extensions [3]: the SPKI is the element just before it.
            spki = prev.take();
            break;
        }
        prev = Some(el);
    }
    // No extensions? then the SPKI is the last element seen.
    let spki = spki.or(prev).ok_or(TlsError::Decode)?;
    if spki.tag != 0x30 {
        return Err(TlsError::Decode);
    }
    // SubjectPublicKeyInfo ::= SEQUENCE { algorithm, subjectPublicKey BIT STRING }
    let mut spki_cur = Der {
        buf: der,
        pos: spki.content_start,
    };
    let alg = spki_cur.read()?;
    let alg_bytes = &der[spki.content_start..alg.tlv_end];
    if alg_bytes != ED25519_ALG_ID {
        return Err(TlsError::UnsupportedCert);
    }
    let key_bs = spki_cur.read()?;
    if key_bs.tag != 0x03 {
        return Err(TlsError::Decode);
    }
    let key_content = &der[key_bs.content_start..key_bs.content_end];
    if key_content.len() != 33 || key_content[0] != 0x00 {
        return Err(TlsError::Decode);
    }
    let mut subject_public_key = [0u8; 32];
    subject_public_key.copy_from_slice(&key_content[1..]);

    Ok(Certificate {
        tbs: tbs_full,
        subject_public_key,
        signature,
    })
}
