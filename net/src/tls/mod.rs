//! rheo-net TLS 1.3 (RFC 8446) - the security transport, Phase N3b
//! (docs/NETSTACK.md §15), built **from scratch** on the N3a crypto primitives
//! (`crate::crypto`): the HKDF key schedule, the ChaCha20-Poly1305 / AES-128-GCM
//! AEAD record layer, X25519 key exchange, Ed25519 signatures, and a minimal
//! X.509 parse. It adds **no kernel object** and **no `cfg(target_arch)`** - pure
//! portable userspace, gated behind the `tls` feature (which implies `crypto`).
//!
//! **Why from-scratch, not rustls** (the architecture decision, docs/NETSTACK.md
//! §15): rustls builds `no_std` on the bare targets, but its public API does not
//! expose the intermediate key-schedule secrets the RFC 8448 known-answer test
//! must check, it generates its own ephemerals (so it cannot be driven with RFC
//! 8448's fixed X25519 keys), and it needs a full custom `CryptoProvider` to wire
//! N3a's primitives in. A from-scratch key schedule over N3a is what the KAT
//! proves, with full control.
//!
//! Scope (N3b): TLS **1.3** only, a full 1-RTT handshake (client + enough server
//! to prove it in-cell), cipher suites `TLS_AES_128_GCM_SHA256` +
//! `TLS_CHACHA20_POLY1305_SHA256`, group **x25519**, signature **ed25519**, and a
//! minimal X.509 (extract SPKI + verify signature). TLS 1.2, full X.509
//! chain/path/name validation, key update, and live HTTPS over the network are
//! **deferred** (N3c/N4, docs/NETSTACK.md §15).
//!
//! Correctness is pinned by the **RFC 8448 §3** trace: the key schedule derives
//! the RFC's secrets/keys/IVs/Finished MACs **byte-for-byte** (`nettls` proof).

pub mod handshake;
pub mod keyschedule;
pub mod msg;
pub mod record;
pub mod x509;

pub use handshake::{HandshakeOutput, ServerIdentity, run_handshake, run_handshake_alpn};
pub use record::{ContentType, RecordKeys};

/// The TLS 1.3 cipher suites this slice supports (RFC 8446 §B.4). Both are
/// SHA-256 suites (the key schedule's hash), differing only in the AEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherSuite {
    /// `TLS_AES_128_GCM_SHA256` (0x1301) - AES-128-GCM, 16-byte key.
    Aes128GcmSha256,
    /// `TLS_CHACHA20_POLY1305_SHA256` (0x1303) - ChaCha20-Poly1305, 32-byte key.
    ChaCha20Poly1305Sha256,
}

impl CipherSuite {
    pub fn to_u16(self) -> u16 {
        match self {
            CipherSuite::Aes128GcmSha256 => 0x1301,
            CipherSuite::ChaCha20Poly1305Sha256 => 0x1303,
        }
    }
    pub fn from_u16(v: u16) -> Option<CipherSuite> {
        match v {
            0x1301 => Some(CipherSuite::Aes128GcmSha256),
            0x1303 => Some(CipherSuite::ChaCha20Poly1305Sha256),
            _ => None,
        }
    }
    /// The AEAD key length in bytes (16 for AES-128-GCM, 32 for ChaCha20-Poly1305).
    pub fn key_len(self) -> usize {
        match self {
            CipherSuite::Aes128GcmSha256 => 16,
            CipherSuite::ChaCha20Poly1305Sha256 => 32,
        }
    }
}

/// A TLS protocol error. Every fault (a bad decode, a failed AEAD, a rejected
/// certificate or Finished MAC) is one of these - the handshake never panics on
/// wire input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsError {
    /// A malformed message / record / DER structure.
    Decode,
    /// AEAD authentication failed opening a record (RFC 8446 `bad_record_mac`).
    BadRecordMac,
    /// No cipher suite in common between client and server.
    NoCommonSuite,
    /// The peer omitted the X25519 key share.
    MissingKeyShare,
    /// The certificate's own signature did not verify.
    BadCertificate,
    /// The CertificateVerify signature did not verify.
    BadSignature,
    /// A Finished `verify_data` MAC did not match.
    BadFinished,
    /// The certificate uses an unsupported key type (only Ed25519 SPKI is parsed).
    UnsupportedCert,
}
